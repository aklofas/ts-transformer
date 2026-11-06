//! The server's post-SETUP read-idle bound derives from what SETUP
//! advertised (`Session: <id>;timeout=N`). With a fixed 30 s timer under a
//! 60 s advertisement, a conformant client pinging at timeout/2 (ours,
//! ffmpeg) loses the race at the second ping; an orphaned fanout (see
//! `orphan_fanout.rs`) would hide it, because media keeps flowing after
//! the reap.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tst_rtp::RtspServerBuilder;

use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp::{header, make_muxer_cfg, request, session_id};

fn interleaved_session(timeout_secs: u64) -> (tst_rtp::RtspServer, TcpStream, String, String) {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.session_timeout(Duration::from_secs(timeout_secs));
    let server = b.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // Per-response read bound only — generous for the slow macOS/Windows
    // runners; the reap test replaces it before it times anything.
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let url = format!("rtsp://127.0.0.1:{port}/live");
    let setup = request(
        &mut tcp,
        &format!(
            "SETUP {url} RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    assert!(
        header(&setup, "Session").ends_with(&format!(";timeout={timeout_secs}")),
        "{setup}"
    );
    let sid = session_id(&setup);
    let play = request(
        &mut tcp,
        &format!("PLAY {url} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "{play}");
    (server, tcp, url, sid)
}

/// With a fixed 30 s timer the connection would stay open for 30 s.
#[test]
fn an_idle_session_is_reaped_at_the_advertised_timeout_plus_grace() {
    let (server, mut tcp, _url, _sid) = interleaved_session(1);
    // No keepalive. The bound is 1 s + max(0.5 s, 2 s) = 3 s; the 20 s
    // deadline (checked at each 1 s read timeout) is far above it and below
    // a fixed 30 s. Neither is scaled: both must stay under the 30 s they
    // tell apart.
    tcp.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 4096];
    loop {
        match tcp.read(&mut buf) {
            Ok(0) => break, // FIN: the server shut the write half
            Ok(_) => {}     // a stray interleaved frame; keep reading
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) =>
            {
                break;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                assert!(
                    Instant::now() < deadline,
                    "connection still open 20 s after PLAY with no keepalive (advertised timeout 1 s)"
                );
            }
            Err(e) => panic!("unexpected read error: {e}"),
        }
    }
    server.stop().ok();
}

/// Guard against over-tightening: a client that keeps pinging inside the
/// advertised timeout stays alive across several ping intervals (10 s
/// advertised → 15 s bound; six pings 1 s apart span ≈ 6 s, so a runner
/// stall of several seconds cannot reach the bound).
#[test]
fn keepalive_pings_inside_the_timeout_keep_the_session_alive() {
    let (server, mut tcp, url, sid) = interleaved_session(10);
    for cseq in 3..9 {
        std::thread::sleep(Duration::from_secs(1));
        let pong = request(
            &mut tcp,
            &format!("OPTIONS {url} RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {sid}\r\n\r\n"),
        );
        assert!(
            pong.starts_with("RTSP/1.0 200"),
            "ping {cseq} was not answered: {pong}"
        );
    }
    let bye = request(
        &mut tcp,
        &format!("TEARDOWN {url} RTSP/1.0\r\nCSeq: 9\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(bye.starts_with("RTSP/1.0 200"), "{bye}");
    server.stop().ok();
}

/// Whether a socket error means the server already closed the connection.
/// macOS answers `setsockopt` on a connection the peer has torn down with
/// `EINVAL` (`InvalidInput`), and some calls on it with `ENOTCONN`.
fn closed_by_peer(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::InvalidInput
            | ErrorKind::NotConnected
    )
}

/// Connect to `port` and set the read timeout before any I/O. `None` when
/// the server closed the connection before the timeout could be set (a
/// peer refused at the session cap can be closed that fast).
fn connect_with_read_timeout(port: u16, timeout: Duration) -> Option<TcpStream> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect to the server");
    match tcp.set_read_timeout(Some(timeout)) {
        Ok(()) => Some(tcp),
        Err(e) if closed_by_peer(e.kind()) => None,
        Err(e) => panic!("setting the read timeout on a fresh connection failed: {e}"),
    }
}

/// Read `tcp` (its read timeout already set, short) until the server
/// closes it or `until` passes; bytes are discarded. True when the server
/// closed it.
fn closed_before(tcp: &mut TcpStream, until: Instant) -> bool {
    let mut buf = [0u8; 4096];
    while Instant::now() < until {
        match tcp.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) if closed_by_peer(e.kind()) => return true,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("unexpected read error: {e}"),
        }
    }
    false
}

/// Write `req` and read (the read timeout already set, short) until a
/// response head arrives, the server closes the connection, or `until`
/// passes. Returns what was read (empty or partial when the server closed
/// or nothing came back in time).
fn try_request(tcp: &mut TcpStream, req: &str, until: Instant) -> String {
    let mut buf = Vec::new();
    if tcp.write_all(req.as_bytes()).is_err() {
        return String::new();
    }
    let mut chunk = [0u8; 1024];
    while Instant::now() < until && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match tcp.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// A connection that never reaches SETUP holds its slot for 30 s however
/// often it sends OPTIONS, then is closed and the slot goes to the next
/// peer. With `max_sessions(1)` a second peer is refused while the first
/// holds it. The 30 s is not scaled (it is the server's fixed bound); the
/// slack above it is.
#[test]
fn a_connection_that_never_sets_up_frees_its_slot_at_the_admission_deadline() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.max_sessions(1);
    let server = b.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let opened = Instant::now();
    let mut holder = connect_with_read_timeout(port, scaled(Duration::from_secs(5)))
        .expect("the first peer was closed before any request");
    let r = request(
        &mut holder,
        &format!("OPTIONS {url} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    );
    assert!(r.starts_with("RTSP/1.0 200"), "{r}");
    // Admitted, so open until the deadline: safe to shorten the timeout for
    // the close probes below.
    holder
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set the holder's probe read timeout");

    // `None`: the server closed the refused peer before its timeout was set.
    let refused_closed =
        connect_with_read_timeout(port, Duration::from_secs(1)).is_none_or(|mut refused| {
            let _ =
                refused.write_all(format!("OPTIONS {url} RTSP/1.0\r\nCSeq: 1\r\n\r\n").as_bytes());
            closed_before(
                &mut refused,
                Instant::now() + scaled(Duration::from_secs(5)),
            )
        });
    assert!(
        refused_closed,
        "a second peer was served while max_sessions(1) was held"
    );

    let give_up = opened + Duration::from_secs(30) + scaled(Duration::from_secs(10));
    let mut cseq = 2;
    let closed = loop {
        if closed_before(
            &mut holder,
            (Instant::now() + Duration::from_secs(10)).min(give_up),
        ) {
            break true;
        }
        if Instant::now() >= give_up {
            break false;
        }
        let _ =
            holder.write_all(format!("OPTIONS {url} RTSP/1.0\r\nCSeq: {cseq}\r\n\r\n").as_bytes());
        cseq += 1;
    };
    let closed_at = opened.elapsed();
    assert!(
        closed,
        "a connection sending only OPTIONS was still open {closed_at:?} after it opened"
    );
    assert!(
        closed_at >= Duration::from_secs(29),
        "closed after {closed_at:?}, before the 30 s deadline"
    );

    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    loop {
        // `None`: refused and closed at once (the slot not free yet); retry.
        let r = connect_with_read_timeout(port, Duration::from_millis(250))
            .map(|mut next| {
                try_request(
                    &mut next,
                    &format!("OPTIONS {url} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
                    deadline,
                )
            })
            .unwrap_or_default();
        if r.starts_with("RTSP/1.0 200") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the freed slot was never handed to a new peer: {r:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    server.stop().ok();
}

/// A session timeout near `u64::MAX` seconds makes the post-SETUP reply
/// bound near `Duration::MAX`; every later reply must still be written.
#[test]
fn setup_with_max_session_timeout_still_replies() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.session_timeout(Duration::from_secs(u64::MAX));
    let server = b.build().unwrap();
    let _mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let url = format!("rtsp://127.0.0.1:{port}/live");
    let setup = request(
        &mut tcp,
        &format!(
            "SETUP {url} RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let sid = session_id(&setup);
    let options = request(
        &mut tcp,
        &format!("OPTIONS {url} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(options.starts_with("RTSP/1.0 200"), "{options}");
    server.stop().ok();
}
