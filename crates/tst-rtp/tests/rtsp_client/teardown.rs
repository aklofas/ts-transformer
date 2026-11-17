use crate::fixtures::rtsp_loopback_server::*;

#[test]
fn drop_sends_teardown_best_effort() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();
    // Drop without explicit teardown — Drop impl should send TEARDOWN.
    drop(client);
    // No assertion — drop completes without panicking.
    drop(h);
}

#[test]
fn explicit_teardown_clears_session() {
    let h = FixtureHandle::spawn(FixtureConfig::default());
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let _s = client.setup_mp2t_auto(&sdp).unwrap();
    client.teardown().unwrap();
    drop(client);
    drop(h);
}

/// A dropped interleaved client on a quiet session still writes TEARDOWN
/// when its `read_timeout` is longer than Drop's 500 ms bound: no read in
/// progress holds the TEARDOWN back.
#[test]
fn drop_writes_teardown_on_a_quiet_interleaved_session_with_a_long_read_timeout() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;
    use crate::fixtures::raw_rtsp_server::{
        answer_describe_and_setup, cseq_of, method_of, read_request,
    };

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = mpsc::channel::<String>();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
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
        // Send nothing more. Record what the client writes next, until a
        // TEARDOWN or the connection ends.
        let mut rest = leftover;
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(&rest).contains("TEARDOWN ") {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&buf[..n]),
            }
        }
        let _ = seen_tx.send(String::from_utf8_lossy(&rest).into_owned());
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .read_timeout(Duration::from_secs(1))
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    // The pause leaves the session quiet for a while before Drop; whichever
    // way that goes, Drop must write TEARDOWN.
    client.play().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(client);

    let seen = seen_rx
        .recv_timeout(scaled(Duration::from_secs(10)))
        .expect("the peer saw a TEARDOWN or the connection end");
    assert!(
        seen.starts_with("TEARDOWN "),
        "the dropped client must write TEARDOWN; the peer saw {seen:?}"
    );
    drop(session);
    peer.join().unwrap();
}

/// What a [`session_peer`] saw: the requests after SETUP, in order.
type Seen = Vec<String>;

/// A peer that answers DESCRIBE and an interleaved SETUP (session
/// `DEADBEEF`), then every later request with `200 OK` except TEARDOWN,
/// which it answers after `teardown_reply` (never when `None`). It reads
/// until the client ends the connection and sends the time it read that
/// end on the returned channel.
fn session_peer(
    teardown_reply: Option<std::time::Duration>,
) -> (
    u16,
    std::sync::mpsc::Receiver<std::time::Instant>,
    std::thread::JoinHandle<Seen>,
) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp_server::{answer_describe_and_setup, cseq_of, method_of};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (eof_tx, eof_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        // Bounds a client that never hangs up; not reached by a passing
        // test.
        sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            while let Some(pos) = leftover.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&leftover[..pos + 4]).into_owned();
                leftover.drain(..pos + 4);
                let reply_after = if method_of(&req) == "TEARDOWN" {
                    teardown_reply
                } else {
                    Some(Duration::ZERO)
                };
                if let Some(after) = reply_after {
                    // The peer is slow on purpose: the reply is what the
                    // client waits for.
                    std::thread::sleep(after);
                    let cseq = cseq_of(&req);
                    let _ = sock.write_all(
                        format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n")
                            .as_bytes(),
                    );
                }
                seen.push(req);
            }
            match sock.read(&mut buf) {
                Ok(0) => {
                    let _ = eof_tx.send(Instant::now());
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                    let _ = eof_tx.send(Instant::now());
                    break;
                }
                Err(_) => break,
                Ok(n) => leftover.extend_from_slice(&buf[..n]),
            }
        }
        seen
    });
    (port, eof_rx, peer)
}

/// A client with a session open on a [`session_peer`].
fn client_with_session(port: u16) -> (tst_rtp::RtspClient, tst_rtp::RtspSession) {
    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    (client, session)
}

fn teardowns(seen: &Seen) -> usize {
    seen.iter().filter(|r| r.starts_with("TEARDOWN ")).count()
}

/// `close` waits for the TEARDOWN's reply and for the client's thread:
/// the peer reads the connection's end right after `close` returns.
#[test]
fn close_awaits_the_teardown_reply_and_ends_the_thread() {
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp::scaled;

    let (port, eof, peer) = session_peer(Some(Duration::from_millis(150)));
    let (client, session) = client_with_session(port);

    let t0 = Instant::now();
    let r = client.close();
    let elapsed = t0.elapsed();

    assert!(r.is_ok(), "got {r:?}");
    assert!(
        elapsed >= Duration::from_millis(150),
        "close returned in {elapsed:?}, before the reply"
    );
    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// A peer that never answers: `close` is `Timeout` after its 500 ms
/// TEARDOWN bound, and still ends the client's thread.
#[test]
fn close_against_a_silent_peer_is_timeout_within_its_bound() {
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp::scaled;
    use tst_rtp::RtspError;

    let (port, eof, peer) = session_peer(None);
    let (client, session) = client_with_session(port);

    let t0 = Instant::now();
    let r = client.close();
    let elapsed = t0.elapsed();

    assert!(matches!(r, Err(RtspError::Timeout)), "got {r:?}");
    assert!(
        elapsed >= Duration::from_millis(400),
        "close returned in {elapsed:?}, before its bound"
    );
    assert!(
        elapsed < scaled(Duration::from_secs(3)),
        "close took {elapsed:?}"
    );
    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// Without a session `close` writes nothing and ends the connection.
#[test]
fn close_without_a_session_sends_nothing() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        // Bounds a client that never hangs up; not reached by a passing
        // test.
        sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let mut seen = Vec::new();
        let r = sock.read_to_end(&mut seen);
        let _ = seen_tx.send((r.is_ok(), seen));
    });

    let client = tst_rtp::RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/test")).unwrap();
    let r = client.close();
    assert!(r.is_ok(), "got {r:?}");

    let (ended, seen) = seen_rx
        .recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    assert!(ended, "the connection did not end cleanly");
    assert!(
        seen.is_empty(),
        "the peer saw {:?}",
        String::from_utf8_lossy(&seen)
    );
    peer.join().unwrap();
}

/// A cancelled client's `close` still writes TEARDOWN, does not wait for
/// the reply, reports the cancel and ends the client's thread.
#[test]
fn close_on_a_cancelled_client_writes_teardown_and_reports_the_cancel() {
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp::scaled;
    use tst_rtp::RtspError;

    let (port, eof, peer) = session_peer(None);
    let (client, session) = client_with_session(port);
    client.cancel_handle().cancel();

    let t0 = Instant::now();
    let r = client.close();
    let elapsed = t0.elapsed();

    assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
    assert!(
        elapsed < scaled(Duration::from_millis(400)),
        "close waited {elapsed:?}: it waited for the TEARDOWN reply"
    );
    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// A client dropped while a panic unwinds writes its TEARDOWN and the
/// unwind reaches `catch_unwind` (a panic in `Drop` would abort).
#[test]
fn drop_during_a_panic_does_not_abort() {
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;

    let (port, eof, peer) = session_peer(Some(Duration::ZERO));
    let (client, session) = client_with_session(port);

    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _client = client;
        panic!("unwinding with a client that has a session");
    }));
    assert!(r.is_err(), "the closure did not panic");

    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// Dropping a client whose cancel handle and receiver are still held:
/// `Drop` returns without waiting for a reply, the peer sees TEARDOWN and
/// the connection's end (nothing the caller still holds keeps the
/// client's thread alive), the receiver's next read returns, and the kept
/// handle reads the cancel.
#[test]
fn drop_with_a_kept_cancel_handle_and_receiver() {
    use std::time::{Duration, Instant};

    use tst_core::transport::RecvTransport;

    use crate::fixtures::raw_rtsp::scaled;

    let (port, eof, peer) = session_peer(None);
    let (mut client, session) = client_with_session(port);
    client.play().unwrap();
    let cancel = client.cancel_handle();
    let mut transport = session.into_recv_transport();
    transport.set_recv_timeout(Some(scaled(Duration::from_secs(5))));

    let t0 = Instant::now();
    drop(client);
    let elapsed = t0.elapsed();

    assert!(
        elapsed < scaled(Duration::from_millis(400)),
        "Drop waited {elapsed:?}"
    );
    assert!(cancel.is_cancelled(), "Drop did not set the cancel");
    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");

    let mut buf = vec![0u8; transport.max_payload()];
    let t0 = Instant::now();
    let r = transport.recv_bytes(&mut buf);
    assert!(r.is_err(), "the receiver read {r:?} after its client ended");
    assert!(
        t0.elapsed() < scaled(Duration::from_secs(1)),
        "the receiver's read waited {:?}",
        t0.elapsed()
    );
    cancel.cancel();
    drop(transport);
}

/// What [`scripted_peer`] answers a request with: the status (`"200 OK"`)
/// and extra header lines, each ending `\r\n`. `None` takes the default.
type Answer = Option<(&'static str, String)>;

/// A peer that answers each request with `answer(request)`, adding the
/// request's CSeq, or by default: DESCRIBE with an SDP, SETUP with an
/// interleaved session `DEADBEEF`, anything else with `200 OK`. It reads
/// until the client ends the connection, sends the time it read that end
/// on the returned channel, and returns every request it read.
fn scripted_peer(
    mut answer: impl FnMut(&str) -> Answer + Send + 'static,
) -> (
    u16,
    std::sync::mpsc::Receiver<std::time::Instant>,
    std::thread::JoinHandle<Seen>,
) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp_server::{SDP_BODY, cseq_of, method_of};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (eof_tx, eof_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        // Bounds a client that never hangs up; not reached by a passing
        // test.
        sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let mut seen = Vec::new();
        let mut leftover = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            while let Some(pos) = leftover.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&leftover[..pos + 4]).into_owned();
                leftover.drain(..pos + 4);
                let (status, extra, body) = match (answer(&req), method_of(&req)) {
                    (Some((status, extra)), _) => (status, extra, &[][..]),
                    (None, "DESCRIBE") => (
                        "200 OK",
                        format!(
                            "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
                            SDP_BODY.len()
                        ),
                        SDP_BODY,
                    ),
                    (None, "SETUP") => (
                        "200 OK",
                        "Session: DEADBEEF;timeout=60\r\n\
                         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n"
                            .to_owned(),
                        &[][..],
                    ),
                    (None, _) => ("200 OK", "Session: DEADBEEF\r\n".to_owned(), &[][..]),
                };
                let cseq = cseq_of(&req);
                let mut reply =
                    format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{extra}\r\n").into_bytes();
                reply.extend_from_slice(body);
                // The client may already have gone.
                let _ = sock.write_all(&reply);
                seen.push(req);
            }
            match sock.read(&mut buf) {
                Ok(0) => {
                    let _ = eof_tx.send(Instant::now());
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                    let _ = eof_tx.send(Instant::now());
                    break;
                }
                Err(_) => break,
                Ok(n) => leftover.extend_from_slice(&buf[..n]),
            }
        }
        seen
    });
    (port, eof_rx, peer)
}

/// A [`scripted_peer`] that answers TEARDOWN with `status` and everything
/// else by default.
fn peer_answering_teardown(
    status: &'static str,
) -> (
    u16,
    std::sync::mpsc::Receiver<std::time::Instant>,
    std::thread::JoinHandle<Seen>,
) {
    scripted_peer(move |req| {
        req.starts_with("TEARDOWN ")
            .then(|| (status, String::new()))
    })
}

/// A TEARDOWN the server refuses is a `Protocol` error carrying the status,
/// and the session is cleared all the same: a second `teardown` sends
/// nothing and is `Ok`.
#[test]
fn teardown_answered_500_is_a_protocol_error_and_clears_the_session() {
    use tst_rtp::RtspError;

    let (port, _eof, peer) = peer_answering_teardown("500 Internal Server Error");
    let (mut client, session) = client_with_session(port);

    let r = client.teardown();
    match &r {
        Err(RtspError::Protocol { code, reason }) => {
            assert_eq!(*code, 500);
            assert_eq!(reason, "Internal Server Error");
        }
        _ => panic!("expected Protocol {{ code: 500, .. }}, got {r:?}"),
    }
    let again = client.teardown();
    assert!(again.is_ok(), "the second teardown returned {again:?}");

    drop(client);
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// `454 Session Not Found` to a TEARDOWN: the session is gone, which is
/// what the TEARDOWN asked for.
#[test]
fn teardown_answered_454_is_ok() {
    let (port, _eof, peer) = peer_answering_teardown("454 Session Not Found");
    let (mut client, session) = client_with_session(port);

    let r = client.teardown();
    assert!(r.is_ok(), "got {r:?}");

    drop(client);
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// The value of `key` in a Digest `Authorization`, quotes removed.
fn digest_param<'a>(authorization: &'a str, key: &str) -> &'a str {
    let at = authorization
        .find(&format!(" {key}="))
        .unwrap_or_else(|| panic!("no {key} in {authorization}"))
        + key.len()
        + 2;
    authorization[at..]
        .split(',')
        .next()
        .unwrap()
        .trim_matches('"')
}

fn md5_hex(s: &str) -> String {
    use md5::{Digest as _, Md5};

    Md5::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The server challenges the TEARDOWN with a new nonce: the client answers
/// it once, with the nonce count restarted for the new nonce, and the
/// accepted retry is `Ok`.
#[test]
fn teardown_answered_401_is_retried_once_with_the_fresh_challenge() {
    use crate::fixtures::raw_rtsp::try_header;

    const N1: &str = r#"Digest realm="R", nonce="n1", qop="auth""#;
    const N2: &str = r#"Digest realm="R", nonce="n2", qop="auth""#;
    let mut teardown_count = 0;
    let (port, _eof, peer) = scripted_peer(move |req| {
        let challenge =
            |www: &str| Some(("401 Unauthorized", format!("WWW-Authenticate: {www}\r\n")));
        if req.starts_with("DESCRIBE ") && try_header(req, "authorization").is_none() {
            return challenge(N1);
        }
        if req.starts_with("TEARDOWN ") {
            teardown_count += 1;
            if teardown_count == 1 {
                return challenge(N2);
            }
        }
        None
    });
    let url = format!("rtsp://u:p@127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();

    let r = client.teardown();
    assert!(r.is_ok(), "got {r:?}");

    drop(client);
    let seen = peer.join().unwrap();
    let tds: Vec<&String> = seen.iter().filter(|r| r.starts_with("TEARDOWN ")).collect();
    assert_eq!(tds.len(), 2, "one TEARDOWN and its retry: {seen:?}");
    let first = try_header(tds[0], "authorization").expect("a pre-emptive Authorization");
    assert_eq!(digest_param(first, "nonce"), "n1");
    let retry = try_header(tds[1], "authorization").expect("the retry's Authorization");
    assert_eq!(digest_param(retry, "nonce"), "n2");
    assert_eq!(digest_param(retry, "nc"), "00000001");
    let uri = tds[1].split_whitespace().nth(1).unwrap();
    assert_eq!(digest_param(retry, "uri"), uri);
    let ha1 = md5_hex("u:R:p");
    let ha2 = md5_hex(&format!("TEARDOWN:{uri}"));
    let cnonce = digest_param(retry, "cnonce");
    assert_eq!(
        digest_param(retry, "response"),
        md5_hex(&format!("{ha1}:n2:00000001:{cnonce}:auth:{ha2}"))
    );
    drop(session);
}

/// A TEARDOWN challenged on a client with no credentials is `AuthFailed`,
/// with no retry.
#[test]
fn teardown_answered_401_without_credentials_is_auth_failed() {
    use tst_rtp::RtspError;

    let (port, _eof, peer) = scripted_peer(|req| {
        req.starts_with("TEARDOWN ").then(|| {
            (
                "401 Unauthorized",
                "WWW-Authenticate: Digest realm=\"R\", nonce=\"n\"\r\n".to_owned(),
            )
        })
    });
    let (mut client, session) = client_with_session(port);

    let r = client.teardown();
    assert!(matches!(r, Err(RtspError::AuthFailed)), "got {r:?}");

    drop(client);
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// A `401` to a TEARDOWN without a `WWW-Authenticate` header is a
/// malformed reply, on a client that has credentials too.
#[test]
fn teardown_answered_401_without_a_challenge_is_bad_response() {
    use tst_rtp::RtspError;

    let (port, _eof, peer) = scripted_peer(|req| {
        req.starts_with("TEARDOWN ")
            .then(|| ("401 Unauthorized", String::new()))
    });
    let url = format!("rtsp://u:p@127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();

    let r = client.teardown();
    assert!(matches!(r, Err(RtspError::BadResponse { .. })), "got {r:?}");

    drop(client);
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// `teardown_within` reports a refused TEARDOWN as `teardown` does.
#[test]
fn teardown_within_reports_the_status_too() {
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;
    use tst_rtp::RtspError;

    let (port, _eof, peer) = peer_answering_teardown("500 Internal Server Error");
    let (mut client, session) = client_with_session(port);

    let r = client.teardown_within(scaled(Duration::from_secs(2)));
    assert!(
        matches!(r, Err(RtspError::Protocol { code: 500, .. })),
        "got {r:?}"
    );

    drop(client);
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// Dropping a client whose TEARDOWN the server answers `500`: `Drop` sets
/// the cancel first, so it never reads the reply and cannot see the status.
/// What this pins is that it does not panic, writes the TEARDOWN once and
/// ends the connection.
#[test]
fn drop_against_a_peer_refusing_teardown_writes_it_and_does_not_panic() {
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;

    let (port, eof, peer) = peer_answering_teardown("500 Internal Server Error");
    let (client, session) = client_with_session(port);

    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(client)));
    assert!(r.is_ok(), "Drop panicked");

    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}

/// `close` returns a refused TEARDOWN's error and still ends the client's
/// thread.
#[test]
fn close_returns_the_teardown_status() {
    use std::time::Duration;

    use crate::fixtures::raw_rtsp::scaled;
    use tst_rtp::RtspError;

    let (port, eof, peer) = peer_answering_teardown("500 Internal Server Error");
    let (client, session) = client_with_session(port);

    let r = client.close();
    assert!(
        matches!(r, Err(RtspError::Protocol { code: 500, .. })),
        "got {r:?}"
    );

    eof.recv_timeout(scaled(Duration::from_secs(1)))
        .expect("the peer read the connection's end");
    let seen = peer.join().unwrap();
    assert_eq!(teardowns(&seen), 1, "the peer saw {seen:?}");
    drop(session);
}
