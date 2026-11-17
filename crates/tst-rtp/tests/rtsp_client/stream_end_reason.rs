//! `EndReason` recorded at every session-death site.
//!
//! `clean_teardown_records_clean_teardown` uses the shared tokio fixture
//! (`fixtures::rtsp_loopback_server`) — its TEARDOWN handler already
//! closes the socket right after replying, which is exactly the
//! orderly-EOF shape `CleanTeardown` covers.
//!
//! The other scenarios (454-answering keepalive, a reset or a close of
//! the control connection on a UDP session, a hard read failure on an
//! interleaved session, and the same read failure observed through
//! `H264Receiver`) need a peer that
//! can force a specific wire outcome the shared fixture's fixed tokio
//! protocol surface has no knob for: answering a keepalive with `454`,
//! or an abortive (`SO_LINGER=0`) close that sends a TCP RST instead of
//! a clean FIN. Those scenarios use the hand-rolled blocking `std::net`
//! responder in `fixtures/raw_rtsp_server.rs`, which completes DESCRIBE +
//! SETUP and leaves the rest of the exchange to the test.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use tst_core::transport::EndReason;
use tst_core::transport::{RecvTransport, TransportError};
use tst_rtp::{H264DepayConfig, RtspClient};

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_server::{
    SDP_BODY, answer_describe_and_setup, cseq_of, force_reset, method_of, read_request,
};
use crate::fixtures::rtp_packets::build_rtp_packet;
use crate::fixtures::rtsp_loopback_server::{FixtureConfig, FixtureHandle, extract_client_port};
use crate::fixtures::wait_for;

/// Clean TEARDOWN: the fixture's TEARDOWN handler writes the 200 OK then
/// returns, ending the tokio task and closing the socket.
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
/// `SessionExpired`, on a UDP session too, with no request of the
/// caller's to read the reply.
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

    wait_for(Duration::from_secs(5), || {
        (!client.is_session_alive()).then_some(())
    });
    client.teardown().unwrap();

    assert!(
        matches!(transport.end_reason(), Some(EndReason::SessionExpired)),
        "expected SessionExpired after a 454 keepalive answer, got {:?}",
        transport.end_reason()
    );

    server.join().unwrap();
    drop(client);
}

/// A reset of the control connection on a UDP session is seen without a
/// request (the control connection is read all the time) and records
/// `TransportFailed`. The server forces an abortive close right after
/// SETUP and never reads again.
#[test]
fn a_reset_control_connection_on_a_udp_session_records_transport_failed() {
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

        // Abortive close. Nothing reads again on this socket.
        ready_rx.recv().unwrap();
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=udp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let transport = session.into_recv_transport();
    ready_tx.send(()).unwrap();

    let reason = wait_for(Duration::from_secs(5), || transport.end_reason());
    assert!(
        matches!(reason, EndReason::TransportFailed { .. }),
        "expected TransportFailed after the control TCP reset, got {reason:?}"
    );

    server.join().unwrap();
    drop(client);
}

/// An abortive close right after an interleaved SETUP's response is a
/// hard read failure on the client's control connection (`TCP read
/// failed`), without any PLAY data.
#[test]
fn a_read_error_on_an_interleaved_session_records_transport_failed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // See the ready-signal comment in `a_reset_control_connection_on_a_udp_session_records_transport_failed`
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

    // The media queue, closed by the stream's end, surfaces the disconnect as
    // Broken on the next recv — the recorded reason above is what lets a
    // caller tell this apart from a clean teardown.
    let mut buf = vec![0u8; 2048];
    let result = transport.recv_timeout(&mut buf, Duration::from_secs(2));
    assert!(
        matches!(result, Err(TransportError::Broken { .. })),
        "expected Broken once the media queue closes, got {result:?}"
    );

    server.join().unwrap();
    drop(client);
}

/// The H.264 path variant of the previous test: after the stream ends,
/// `recv_au()` surfaces the usual `Ok(None)` clean-EOS shape — but
/// `end_reason()` lets the caller tell a genuine wire failure apart from
/// an orderly teardown, which `Ok(None)` alone cannot.
#[test]
fn h264_stream_death_records_transport_failed_and_recv_au_returns_none() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // See the ready-signal comment in `a_reset_control_connection_on_a_udp_session_records_transport_failed`
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

    // recv_au() blocks until the media queue closes (post-reset), then
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

/// The peer closes the control connection after a UDP SETUP: with no
/// request made, the client sees the close, reports the session dead and
/// records `CleanTeardown`.
#[test]
fn a_peer_close_on_a_udp_session_marks_the_session_dead() {
    use crate::fixtures::raw_rtsp::scaled;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // The close waits until SETUP's reply was read by the client.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "SETUP", "unexpected request: {req}");
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
        ready_rx.recv().unwrap();
        drop(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=udp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = tst_rtp::Sdp::parse(SDP_BODY).unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let transport = session.into_recv_transport();
    assert!(client.is_session_alive());
    ready_tx.send(()).unwrap();

    // The dead flag is set just before the reason is recorded: wait for both.
    let reason = wait_for(scaled(Duration::from_secs(2)), || {
        (!client.is_session_alive())
            .then(|| transport.end_reason())
            .flatten()
    });
    assert_eq!(reason, EndReason::CleanTeardown);

    server.join().unwrap();
    drop(client);
}

/// Peer for the dropped-client tests: answer DESCRIBE, an interleaved
/// SETUP and PLAY, write `media` (already framed), then keep the
/// connection open until the client closes it: answer every OPTIONS with
/// 200 and leave every other request (the dropped client's TEARDOWN)
/// unanswered.
fn spawn_interleaved_peer(media: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut sock, _): (TcpStream, _) = listener.accept().unwrap();
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
        sock.write_all(&media).unwrap();
        let mut buf = [0u8; 4096];
        loop {
            while let Some(pos) = leftover.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&leftover[..pos + 4]).into_owned();
                leftover.drain(..pos + 4);
                if method_of(&req) == "OPTIONS" {
                    let cseq = cseq_of(&req);
                    sock.write_all(
                        format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: OPTIONS\r\n\r\n")
                            .as_bytes(),
                    )
                    .unwrap();
                }
            }
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => leftover.extend_from_slice(&buf[..n]),
            }
        }
    });
    (port, peer)
}

/// One RTP packet as a TCP-interleaved `$`-frame on channel 0.
fn interleaved_rtp_frame(rtp_pkt: &[u8]) -> Vec<u8> {
    let len = u16::try_from(rtp_pkt.len()).unwrap();
    let mut frame = vec![0x24u8, 0];
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(rtp_pkt);
    frame
}

/// Dropping the owning client ends its thread with `Cancelled` recorded;
/// the transport reports that as the cancel, not as a broken wire. The
/// peer keeps the connection open, so nothing but the drop can end the
/// media queue. The transport is then ended like after `close()`: a
/// second read answers `Closed` and it is no longer alive. The persistent
/// deadline is a safety net only: a regression fails with `Backpressure`
/// instead of hanging.
#[test]
fn dropping_the_client_ends_an_interleaved_recv_transport_as_a_cancel() {
    let (port, peer) = spawn_interleaved_peer(Vec::new());

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    client.play().unwrap();
    let mut transport = session.into_recv_transport();
    transport.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

    drop(client);

    let mut buf = vec![0u8; transport.max_payload()];
    let result = transport.recv_bytes(&mut buf);
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "a dropped client must end the transport as a cancel, got {result:?}"
    );
    assert!(
        matches!(transport.end_reason(), Some(EndReason::Cancelled)),
        "expected Cancelled, got {:?}",
        transport.end_reason()
    );
    let second = transport.recv_bytes(&mut buf);
    assert!(
        matches!(second, Err(TransportError::Closed)),
        "a second read after the cancel must answer Closed, got {second:?}"
    );
    assert!(
        !transport.is_alive(),
        "the ended transport must not be alive"
    );

    drop(transport);
    peer.join().unwrap();
}

/// Dropping the owning client ends an H.264 receiver as a cancel, after
/// the access units it holds: the complete one, then the open one a flush
/// completes.
///
/// The peer sends two single-NALU access units after PLAY: AU1 with the
/// marker bit (complete on its own) and AU2 without it (open until a new
/// timestamp or a flush). Then the test sends OPTIONS; the peer answers it
/// after the media on the same connection, and the client's thread reads
/// that connection in order, so when `options()` returns both media packets are
/// in the receiver's queue. Nothing has been read from it yet and nothing
/// else is sent. The client is dropped at that point. Later calls hand out
/// AU1 (marker), then reach the drained queue's end, whose recorded reason
/// is `Cancelled`: the flush yields AU2, then every call is the cancel.
#[test]
fn dropping_the_client_ends_an_h264_receiver_as_a_cancel_after_its_queued_units() {
    const PT: u8 = 96;
    const SSRC: u32 = 0x5EED_0001;
    let au1 = [0x41u8, 0x01, 0x02];
    let au2 = [0x41u8, 0x03, 0x04];
    let mut media = interleaved_rtp_frame(&build_rtp_packet(1, 90_000, SSRC, PT, true, &au1));
    media.extend(interleaved_rtp_frame(&build_rtp_packet(
        2, 93_003, SSRC, PT, false, &au2,
    )));
    let (port, peer) = spawn_interleaved_peer(media);

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    client.play().unwrap();
    // H264DepayConfig is non-exhaustive — default-and-assign.
    let mut h264_config = H264DepayConfig::default();
    h264_config.payload_type = PT;
    let mut receiver = session.into_h264_receiver(h264_config);
    receiver.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    client.options().unwrap();

    drop(client);

    let first = receiver.recv_au().expect("AU1").expect("AU1 is queued");
    assert_eq!(first.annexb, [0, 0, 0, 1, 0x41, 0x01, 0x02], "AU1 mismatch");
    let second = receiver
        .recv_au()
        .expect("AU2")
        .expect("the flush completes AU2");
    assert_eq!(
        second.annexb,
        [0, 0, 0, 1, 0x41, 0x03, 0x04],
        "AU2 mismatch"
    );
    for call in 0..2 {
        let result = receiver.recv_au();
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "call {call} after the flushed unit must report the cancel, got {result:?}"
        );
    }
    assert!(
        matches!(receiver.end_reason(), Some(EndReason::Cancelled)),
        "expected Cancelled, got {:?}",
        receiver.end_reason()
    );

    drop(receiver);
    peer.join().unwrap();
}

/// Peer for the accepted-TEARDOWN tests: answer DESCRIBE and an
/// interleaved SETUP, then answer every request with 200. After the
/// TEARDOWN's reply it closes the connection when `close_after_teardown`
/// is set (its FIN then races the client's drop); otherwise it keeps the
/// connection open until the client closes it.
fn spawn_teardown_peer(close_after_teardown: bool) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut sock, _): (TcpStream, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        let mut buf = [0u8; 4096];
        loop {
            while let Some(pos) = leftover.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&leftover[..pos + 4]).into_owned();
                leftover.drain(..pos + 4);
                let cseq = cseq_of(&req);
                sock.write_all(
                    format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n")
                        .as_bytes(),
                )
                .unwrap();
                if method_of(&req) == "TEARDOWN" && close_after_teardown {
                    return;
                }
            }
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => leftover.extend_from_slice(&buf[..n]),
            }
        }
    });
    (port, peer)
}

/// `teardown()` and then dropping the client end a live interleaved
/// transport with end of stream, whether or not the peer's FIN after its
/// TEARDOWN reply was read before the drop: the accepted TEARDOWN records
/// `CleanTeardown` and closes the media queue before `teardown()` returns.
/// Fifty fresh sessions, so a race between the FIN and the drop would show.
#[test]
fn teardown_then_drop_ends_an_interleaved_recv_transport_with_end_of_stream() {
    for run in 0..50 {
        let (port, peer) = spawn_teardown_peer(true);
        let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
        let mut client = RtspClient::connect(&url).unwrap();
        let sdp = client.describe().unwrap();
        let session = client.setup_mp2t_auto(&sdp).unwrap();
        let mut transport = session.into_recv_transport();
        // A safety net only: a regression fails with `Backpressure`.
        transport.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

        client.teardown().unwrap();
        drop(client);

        let mut buf = vec![0u8; transport.max_payload()];
        let result = transport.recv_bytes(&mut buf);
        assert!(
            matches!(result, Err(TransportError::Closed)),
            "run {run}: an accepted TEARDOWN must end the transport with end of stream, got {result:?}"
        );
        assert_eq!(
            transport.end_reason(),
            Some(EndReason::CleanTeardown),
            "run {run}"
        );
        peer.join().unwrap();
    }
}

/// The H.264 twin: after `teardown()` and the drop, `recv_au()` is
/// `Ok(None)` on every run, never the cancel.
#[test]
fn teardown_then_drop_ends_an_h264_receiver_with_end_of_stream() {
    for run in 0..50 {
        let (port, peer) = spawn_teardown_peer(true);
        let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
        let mut client = RtspClient::connect(&url).unwrap();
        let sdp = client.describe().unwrap();
        let session = client.setup_mp2t_auto(&sdp).unwrap();
        // H264DepayConfig is non-exhaustive — default-and-assign.
        let mut h264_config = H264DepayConfig::default();
        h264_config.payload_type = 96;
        let mut receiver = session.into_h264_receiver(h264_config);
        receiver.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

        client.teardown().unwrap();
        drop(client);

        let result = receiver.recv_au();
        assert!(
            matches!(result, Ok(None)),
            "run {run}: an accepted TEARDOWN must end the receiver with end of stream, got {result:?}"
        );
        assert_eq!(
            receiver.end_reason(),
            Some(EndReason::CleanTeardown),
            "run {run}"
        );
        peer.join().unwrap();
    }
}

/// The TEARDOWN's `200` alone ends the receiver: the peer keeps the
/// connection open and the client is not dropped, so no FIN and no cancel
/// can be what ends it.
#[test]
fn an_accepted_teardown_ends_the_receiver_while_the_peer_keeps_the_connection_open() {
    let (port, peer) = spawn_teardown_peer(false);
    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();

    client.teardown().unwrap();

    let mut buf = vec![0u8; transport.max_payload()];
    let result = transport.recv_timeout(&mut buf, scaled(Duration::from_secs(5)));
    assert!(
        matches!(result, Err(TransportError::Closed)),
        "the TEARDOWN's reply must end the transport with end of stream, got {result:?}"
    );
    assert_eq!(transport.end_reason(), Some(EndReason::CleanTeardown));

    drop(client);
    peer.join().unwrap();
}

/// Dropping the client is a cancel even when the server answers the drop's
/// own TEARDOWN at once: `Drop` sets the cancel before it writes the
/// TEARDOWN, and an accepted TEARDOWN on a cancelled client records the
/// cancel. Three hundred fresh sessions against the loopback server, which
/// answers TEARDOWN as soon as it reads it.
#[test]
fn dropping_the_client_is_a_cancel_even_when_the_server_answers_its_teardown() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    for run in 0..300 {
        let url = format!("rtsp://127.0.0.1:{}/test?transport=tcp", h.port);
        let mut client = RtspClient::connect(&url).unwrap();
        let sdp = client.describe().unwrap();
        let session = client.setup_mp2t_auto(&sdp).unwrap();
        let mut transport = session.into_recv_transport();
        // A safety net only: a regression fails with `Backpressure`.
        transport.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

        drop(client);

        let mut buf = vec![0u8; transport.max_payload()];
        let result = transport.recv_bytes(&mut buf);
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "run {run}: a dropped client must end the transport as a cancel, got {result:?}"
        );
        assert_eq!(
            transport.end_reason(),
            Some(EndReason::Cancelled),
            "run {run}"
        );
    }
}

/// What a scripted peer does with the TEARDOWN it reads.
#[derive(Clone, Copy)]
enum OnTeardown {
    /// No reply; the connection stays open until the client closes it.
    Ignore,
    /// The `200` and bytes that break the message framing, in one write.
    AnswerThenGarbage,
    /// Close the connection without a reply.
    Close,
    /// Reset the connection without a reply.
    Reset,
    /// Refuse it with `405`; the connection stays open.
    Refuse,
}

/// A peer that answers DESCRIBE and an interleaved SETUP, answers every
/// other request but TEARDOWN with `200`, and handles TEARDOWN as `on`
/// says. Unless it closed or reset, it reads until the client closes.
fn spawn_scripted_peer(on: OnTeardown) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut sock, _): (TcpStream, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        let mut buf = [0u8; 4096];
        loop {
            while let Some(pos) = leftover.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&leftover[..pos + 4]).into_owned();
                leftover.drain(..pos + 4);
                let cseq = cseq_of(&req);
                if method_of(&req) != "TEARDOWN" {
                    sock.write_all(
                        format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n")
                            .as_bytes(),
                    )
                    .unwrap();
                    continue;
                }
                match on {
                    OnTeardown::Ignore => {}
                    OnTeardown::AnswerThenGarbage => {
                        let mut out =
                            format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n")
                                .into_bytes();
                        out.extend_from_slice(
                            b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: nope\r\n\r\n",
                        );
                        // The client may already have closed on the garbage.
                        let _ = sock.write_all(&out);
                    }
                    OnTeardown::Close => return,
                    OnTeardown::Reset => {
                        force_reset(sock);
                        return;
                    }
                    OnTeardown::Refuse => {
                        sock.write_all(
                            format!("RTSP/1.0 405 Method Not Allowed\r\nCSeq: {cseq}\r\n\r\n")
                                .as_bytes(),
                        )
                        .unwrap();
                    }
                }
            }
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => leftover.extend_from_slice(&buf[..n]),
            }
        }
    });
    (port, peer)
}

/// A fresh interleaved session against `port`: the client and its
/// transport, with a scaled safety-net deadline on the transport.
fn interleaved_session(port: u16) -> (RtspClient, tst_rtp::RtpRecvTransport) {
    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    let mut transport = session.into_recv_transport();
    // A safety net only: a regression fails with `Backpressure`.
    transport.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    (client, transport)
}

/// Drop the client under a live transport, `runs` times against a peer
/// doing `on`; every run must end the transport as a cancel. Without the
/// cancel recorded by `Drop` the wrong end shows in a few runs in a
/// hundred to a few in a thousand, depending on the peer; the counts are
/// sized to that.
fn assert_drop_is_a_cancel(on: OnTeardown, runs: usize) {
    for run in 0..runs {
        let (port, peer) = spawn_scripted_peer(on);
        let (client, mut transport) = interleaved_session(port);
        drop(client);
        let mut buf = vec![0u8; transport.max_payload()];
        let result = transport.recv_bytes(&mut buf);
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "run {run}: a dropped client must end the transport as a cancel, got {result:?}"
        );
        assert_eq!(
            transport.end_reason(),
            Some(EndReason::Cancelled),
            "run {run}"
        );
        drop(transport);
        peer.join().unwrap();
    }
}

/// The server answers the drop's TEARDOWN with `200` and, in the same
/// write, bytes that break the framing: still the cancel.
#[test]
fn dropping_the_client_is_a_cancel_even_when_the_server_answers_its_teardown_and_then_sends_garbage()
 {
    assert_drop_is_a_cancel(OnTeardown::AnswerThenGarbage, 300);
}

/// The server closes the connection on the drop's TEARDOWN without a
/// reply: still the cancel, not the peer's clean end.
#[test]
fn dropping_the_client_is_a_cancel_even_when_the_server_closes_without_answering_its_teardown() {
    assert_drop_is_a_cancel(OnTeardown::Close, 600);
}

/// The server resets the connection on the drop's TEARDOWN: still the
/// cancel, not a transport failure.
#[test]
fn dropping_the_client_is_a_cancel_even_when_the_server_resets_on_its_teardown() {
    assert_drop_is_a_cancel(OnTeardown::Reset, 1000);
}

/// The drop records the cancel before it returns: with a peer that never
/// answers and keeps the connection open, the reason is there at once,
/// whether or not the client's thread has seen the stop yet.
#[test]
fn the_reason_is_recorded_when_drop_returns() {
    for run in 0..100 {
        let (port, peer) = spawn_scripted_peer(OnTeardown::Ignore);
        let (client, transport) = interleaved_session(port);
        drop(client);
        assert_eq!(
            transport.end_reason(),
            Some(EndReason::Cancelled),
            "run {run}"
        );
        drop(transport);
        peer.join().unwrap();
    }
}

/// `close()` with an accepted TEARDOWN ends both kinds of receiver with
/// end of stream and `CleanTeardown`. The peer keeps the connection open,
/// so only the TEARDOWN's reply can end them.
#[test]
fn close_ends_both_receivers_with_end_of_stream() {
    let (port, peer) = spawn_teardown_peer(false);
    let (client, mut transport) = interleaved_session(port);
    client.close().unwrap();
    let mut buf = vec![0u8; transport.max_payload()];
    let result = transport.recv_bytes(&mut buf);
    assert!(
        matches!(result, Err(TransportError::Closed)),
        "an accepted TEARDOWN through close() must be end of stream, got {result:?}"
    );
    assert_eq!(transport.end_reason(), Some(EndReason::CleanTeardown));
    peer.join().unwrap();

    let (port, peer) = spawn_teardown_peer(false);
    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    // H264DepayConfig is non-exhaustive — default-and-assign.
    let mut h264_config = H264DepayConfig::default();
    h264_config.payload_type = 96;
    let mut receiver = session.into_h264_receiver(h264_config);
    receiver.set_recv_timeout(Some(scaled(Duration::from_secs(5))));
    client.close().unwrap();
    let result = receiver.recv_au();
    assert!(
        matches!(result, Ok(None)),
        "an accepted TEARDOWN through close() must be end of stream, got {result:?}"
    );
    assert_eq!(receiver.end_reason(), Some(EndReason::CleanTeardown));
    peer.join().unwrap();
}

/// `close()` against a peer that refuses the TEARDOWN returns the refusal,
/// then stops the client's thread: the receiver ends as a cancel.
#[test]
fn close_against_a_peer_refusing_teardown_ends_the_receiver_as_a_cancel() {
    let (port, peer) = spawn_scripted_peer(OnTeardown::Refuse);
    let (client, mut transport) = interleaved_session(port);
    let r = client.close();
    assert!(
        matches!(r, Err(tst_rtp::RtspError::Protocol { .. })),
        "a refused TEARDOWN is reported, got {r:?}"
    );
    let mut buf = vec![0u8; transport.max_payload()];
    let result = transport.recv_bytes(&mut buf);
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "a refused TEARDOWN then the stop must end the transport as a cancel, got {result:?}"
    );
    assert_eq!(transport.end_reason(), Some(EndReason::Cancelled));
    drop(transport);
    peer.join().unwrap();
}
