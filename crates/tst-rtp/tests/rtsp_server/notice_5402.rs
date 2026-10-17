//! RFC 7826 §13.5.1 Notice 5402
//! ("Server-Initiated TEARDOWN") wire delivery.
//!
//! Drives a raw TCP client through OPTIONS / DESCRIBE / SETUP / PLAY
//! against a real `RtspServer`, then calls `server.stop()` on a worker
//! thread (so `stop()`'s drain sleep doesn't block this test thread)
//! and reads the bytes the server pushes out before the TCP closes.
//! Asserts the ANNOUNCE arrives with `Notice: 5402`.
//!
//! We can't use `RtspClient` here: it's a request/response client and
//! has no path to surface a server-initiated ANNOUNCE to the caller.

use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tst_rtp::RtspServer;

use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, session_id};

/// Read from `stream` until `terminator` appears anywhere in the
/// accumulated buffer, EOF, or `deadline` passes. Returns the
/// accumulated bytes (which may not contain `terminator` if we hit EOF
/// or timeout). Uses a short per-read timeout so we don't block the
/// whole way to `deadline` after the peer goes idle.
fn read_until(stream: &mut TcpStream, terminator: &[u8], deadline: Instant) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut buf = Vec::with_capacity(2048);
    while Instant::now() < deadline {
        let mut chunk = [0u8; 1024];
        match stream.read(&mut chunk) {
            Ok(0) => break, // EOF — peer closed.
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(terminator.len()).any(|w| w == terminator) {
                    return buf;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    buf
}

#[test]
fn stop_sends_notice_5402_announce_to_active_session() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    // Connect a raw TCP client + drive OPTIONS → DESCRIBE → SETUP →
    // PLAY by hand. We use `?transport=tcp` so SETUP completes without
    // the server needing to allocate UDP sockets (which would expand
    // the test's port surface).
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_nodelay(true).unwrap();
    tcp.set_read_timeout(Some(crate::fixtures::raw_rtsp::scaled(
        Duration::from_secs(2),
    )))
    .unwrap();

    let opts = request(
        &mut tcp,
        &format!("OPTIONS rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    );
    assert!(opts.contains("200 OK"), "OPTIONS failed: {opts}");

    let describe = request(
        &mut tcp,
        &format!(
            "DESCRIBE rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n"
        ),
    );
    assert!(describe.contains("200 OK"), "DESCRIBE failed: {describe}");

    let setup = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup.contains("200 OK"), "SETUP failed: {setup}");

    // Extract the Session: header so we can assert it round-trips into
    // the ANNOUNCE.
    let session_id = session_id(&setup);

    let play = request(
        &mut tcp,
        &format!(
            "PLAY rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\n\r\n"
        ),
    );
    assert!(play.contains("200 OK"), "PLAY failed: {play}");

    // Kick stop() off on a worker thread — stop() blocks for the full
    // graceful drain (~1.1 s by default), and we want to read the
    // server's outbound bytes from this thread while that's happening.
    // We can't `drop(server)` from another thread without moving it;
    // wrap in an Arc so both threads can observe.
    let server = std::sync::Arc::new(server);
    let server_clone = server.clone();
    let stop_thread = std::thread::spawn(move || {
        server_clone.stop().unwrap();
    });

    // Now read whatever bytes the server pushes before TCP close.
    // stop() should have queued the ANNOUNCE before this read starts;
    // we give ourselves up to 3 s of read budget.
    let deadline = Instant::now() + Duration::from_secs(3);
    let buf = read_until(&mut tcp, b"\r\n\r\n", deadline);
    let text = String::from_utf8_lossy(&buf);

    stop_thread.join().unwrap();

    // Assertions on the received bytes.
    assert!(
        text.starts_with("ANNOUNCE "),
        "expected ANNOUNCE start-line; got: {text:?}"
    );
    assert!(
        text.contains("Notice: 5402"),
        "expected `Notice: 5402` header; got: {text:?}"
    );
    assert!(
        text.contains("Server-Initiated TEARDOWN"),
        "expected Notice reason phrase; got: {text:?}"
    );
    assert!(
        text.contains(&format!("Session: {session_id}")),
        "expected Session: {session_id}; got: {text:?}"
    );
    assert!(
        text.contains("/live"),
        "expected mount path /live in request URI; got: {text:?}"
    );
    assert!(
        text.contains("CSeq:"),
        "expected server-allocated CSeq; got: {text:?}"
    );

    // Drop the server reference so its Drop hard-cancels the runtime.
    drop(server);
}

/// After `server.stop()` returns, the per-session TCP write half is
/// explicitly shut down (FIN sent), so the client's next read
/// observes EOF promptly instead of timing out.
///
/// Cancelling the per-session task drops only the read half — the write
/// half stays open through Arc clones held by `state.sessions` and the
/// fanout task — so without the explicit shutdown loop in `stop()` the
/// client's pump keeps timing out on reads until its own teardown
/// deadline. This test pins the EOF-arrives-promptly behavior.
#[test]
fn stop_shuts_down_per_session_write_half_so_client_sees_eof_promptly() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();

    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_nodelay(true).unwrap();
    tcp.set_read_timeout(Some(crate::fixtures::raw_rtsp::scaled(
        Duration::from_secs(2),
    )))
    .unwrap();
    // Drive through SETUP so the server registers an ActiveSession with
    // a populated `tcp_write` — the new shutdown loop only fires on
    // sessions present in the registry at stop() time.
    let _ = request(
        &mut tcp,
        &format!("OPTIONS rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    );
    let _ = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );

    // Call stop() in the foreground — it blocks for the full graceful
    // drain (~1.1 s default + 500 ms shutdown timeout per session)
    // before returning.
    server.stop().unwrap();

    // After stop() returns, the server should have shutdown the write
    // half — so a read here should return Ok(0) (EOF) within a tight
    // budget (50 ms is conservative; FIN should have already landed).
    tcp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut buf = [0u8; 1024];
    let started = Instant::now();
    loop {
        match tcp.read(&mut buf) {
            Ok(0) => break, // FIN observed — fix is working.
            Ok(_) => {
                // The notice 5402 ANNOUNCE arrived first; keep reading
                // until we hit FIN (which the explicit shutdown guarantees).
                continue;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                panic!(
                    "expected EOF on read after server.stop() — got read-timeout after \
                     {:?}. The server-side per-session TCP write half is leaking open \
                     (regression: removing the explicit shutdown loop in RtspServer::stop?)",
                    started.elapsed()
                );
            }
            Err(_) => break, // Connection reset is also acceptable.
        }
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "EOF observation took too long: {:?}",
        started.elapsed()
    );
}

#[test]
fn stop_without_active_sessions_is_clean_noop_path() {
    // Smoke test: stop() with zero active sessions should not iterate
    // anything, not panic, and complete in graceful_shutdown_drain.
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let started = Instant::now();
    server.stop().unwrap();
    // Loose upper bound — drain is 100ms + 1s safety buffer in the
    // default builder + per-session timeouts (none here).
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "stop() with no sessions took too long: {:?}",
        started.elapsed()
    );
}
