//! Verifies the client's keepalive emits OPTIONS pings on
//! the control TCP within `session_timeout / 2` (or the explicit
//! override), and that dropping the client then ends cleanly.
//!
//! Uses a hand-rolled TCP `accept()` loop instead of a full RTSP server
//! — we only care about observing the wire format the keepalive emits.

use std::io::{Read, Write};
use std::net::TcpListener;

#[test]
fn keepalive_pings_within_session_timeout() {
    // Bind a loopback listener and capture the port for the URL.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Background server: accept one connection, expect the keepalive's
    // OPTIONS ping at CSeq 1000001 within ~10 s, then reply 200 OK.
    let h = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let mut buf = vec![0u8; 4096];
        let mut total = String::new();
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            let n = match sock.read(&mut buf) {
                Ok(n) => n,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            total.push_str(std::str::from_utf8(&buf[..n]).unwrap_or(""));
            // The encoder canonicalizes header names via per-segment
            // first-letter capitalization — `cseq` renders as `Cseq:`
            // (not `CSeq:`). Match either spelling case-insensitively
            // since RTSP headers are case-insensitive per RFC 2326 §12.
            let lower = total.to_ascii_lowercase();
            if lower.contains("options") && lower.contains("cseq: 1000001") {
                // Reply 200 OK and return.
                let _ = sock.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1000001\r\n\r\n");
                return;
            }
        }
        panic!("no OPTIONS ping seen within 10 s; got: {total}");
    });

    // Build the client and drive the keepalive via the lower-level
    // `spawn_keepalive_if_needed` helper directly.
    let url = format!("rtsp://127.0.0.1:{port}/test");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    client
        .spawn_keepalive_if_needed(Some(std::time::Duration::from_secs(2)))
        .unwrap();

    // Hold the client for 5 s — at a 2 s cadence the keepalive
    // emits its first ping at ~t+2 s, well inside the 10 s budget.
    std::thread::sleep(std::time::Duration::from_secs(5));
    drop(client);
    h.join().unwrap();
}

/// A 200 OK in the keepalive CSeq range (≥ 1_000_000) that answers no
/// ping in flight can arrive ahead of a request's reply, on a UDP session
/// or before SETUP too. It must be dropped by its CSeq — returning it
/// would misattribute it as the response to whatever request the caller
/// just sent. Here the stale keepalive 200
/// carries no `Public:` header, so a misattributing `options()` would return an empty
/// method list from the wrong response.
#[test]
fn stale_keepalive_response_not_misattributed_without_interleaving() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let h = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        // Unsolicited keepalive-CSeq 200 — lands in the client's socket
        // buffer before its OPTIONS request's real response.
        sock.write_all(b"RTSP/1.0 200 OK\r\nCseq: 1000005\r\n\r\n")
            .unwrap();
        // Read the client's OPTIONS request, echo its CSeq back.
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let mut total = String::new();
        while !total.contains("\r\n\r\n") {
            let n = sock.read(&mut buf).unwrap();
            assert!(n > 0, "client hung up before sending OPTIONS");
            total.push_str(std::str::from_utf8(&buf[..n]).unwrap_or(""));
        }
        let cseq = total
            .to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("cseq:").map(|v| v.trim().to_string()))
            .expect("client request carries a CSeq header");
        sock.write_all(
            format!("RTSP/1.0 200 OK\r\nCseq: {cseq}\r\nPublic: OPTIONS, DESCRIBE\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
        // Hold the socket open until the client is done reading.
        let _ = sock.read(&mut buf);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let opts = client.options().unwrap();
    drop(client);
    h.join().unwrap();
    assert!(
        opts.public_methods.iter().any(|m| m == "DESCRIBE"),
        "options() must return the response matching its own CSeq, not the \
         stale keepalive 200 sitting ahead of it (got Public: {:?})",
        opts.public_methods
    );
}

/// A sub-200 ms `keepalive_interval` override must be honored, not
/// silently quantized up to the thread's cancel-poll granularity. The
/// keepalive loop used to sleep a fixed 200 ms per cancel check, so any
/// requested cadence below that floor degraded to ~200 ms: in that
/// regime NO two pings can ever reach the wire less than 200 ms apart.
/// Honoring a 25 ms interval puts consecutive pings tens of ms apart.
///
/// The discriminator is therefore the SHORTEST inter-arrival gap the
/// server observes, not a ping count over a wall-clock window. A count
/// (`≥10 in 1.5 s`) flaked twice on macOS runners, which stretch short
/// sleeps under load (saw 9 — a 25 ms request averaging 167 ms); a
/// stretched sleeper still produces plenty of sub-150 ms gaps, whereas
/// the quantized regime cannot produce even one. Only reads that carry
/// exactly one ping bound a gap: a read holding several pings says only
/// that the reader fell behind, so it breaks the chain instead of
/// counting as evidence either way.
#[test]
fn keepalive_honors_sub_200ms_interval() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Record (arrival instant, pings in that read) per read until the
    // client drops (read returns EOF). No responses are written — the
    // keepalive is write-only and the arrivals are the only observable
    // this test needs.
    let h = std::thread::spawn(move || -> Vec<(std::time::Instant, usize)> {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 4096];
        let mut reads = Vec::new();
        loop {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let at = std::time::Instant::now();
                    let pings = std::str::from_utf8(&buf[..n])
                        .unwrap_or("")
                        .matches("OPTIONS rtsp")
                        .count();
                    reads.push((at, pings));
                }
            }
        }
        reads
    });

    let url = format!("rtsp://127.0.0.1:{port}/test");
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    client
        .spawn_keepalive_if_needed(Some(std::time::Duration::from_millis(25)))
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1500));
    drop(client);
    let reads = h.join().unwrap();
    let pings: usize = reads.iter().map(|(_, n)| n).sum();
    // Gaps between consecutive single-ping reads only (see the doc).
    let gaps: Vec<std::time::Duration> = reads
        .windows(2)
        .filter(|w| w[0].1 == 1 && w[1].1 == 1)
        .map(|w| w[1].0.duration_since(w[0].0))
        .collect();
    let shortest = gaps.iter().min().copied();
    assert!(
        shortest.is_some_and(|g| g < std::time::Duration::from_millis(150)),
        "expected consecutive OPTIONS pings under 150 ms apart at a 25 ms interval \
         (the quantized 200 ms regime cannot produce one); saw {pings} pings, \
         shortest single-ping gap {shortest:?}, gaps {gaps:?}"
    );
}

/// The pings stop once TEARDOWN is sent: the peer sees pings flowing,
/// answers the TEARDOWN, and then reads no OPTIONS for three intervals
/// while the client is still alive.
#[test]
fn teardown_ends_the_pings() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crate::fixtures::raw_rtsp_server::{
        answer_describe_and_setup, cseq_of, method_of, read_request,
    };

    const INTERVAL: Duration = Duration::from_millis(50);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (pinging_tx, pinging_rx) = mpsc::channel::<()>();
    let (after_tx, after_rx) = mpsc::channel::<String>();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(
            &mut sock,
            &mut leftover,
            "RTP/AVP/TCP;unicast;interleaved=0-1",
        );
        let mut pings = 0;
        loop {
            let req = read_request(&mut sock, &mut leftover);
            let cseq = cseq_of(&req);
            sock.write_all(
                format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: DEADBEEF\r\n\r\n").as_bytes(),
            )
            .unwrap();
            match method_of(&req) {
                "OPTIONS" => {
                    pings += 1;
                    if pings == 2 {
                        let _ = pinging_tx.send(());
                    }
                }
                "TEARDOWN" => break,
                other => panic!("unexpected request: {other}"),
            }
        }
        // Everything the client writes for three intervals after the reply.
        let mut after = leftover;
        let end = Instant::now() + INTERVAL * 3;
        let mut buf = [0u8; 4096];
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            sock.set_read_timeout(Some(left)).unwrap();
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => after.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let _ = after_tx.send(String::from_utf8_lossy(&after).into_owned());
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .no_auto_keepalive(true)
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let session = client.setup_mp2t_auto(&sdp).unwrap();
    client.spawn_keepalive_if_needed(Some(INTERVAL)).unwrap();
    pinging_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the peer saw two pings");
    client.teardown().unwrap();
    let after = after_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the peer's report");
    assert!(
        !after.contains("OPTIONS"),
        "a ping followed the TEARDOWN: {after:?}"
    );

    drop(session);
    drop(client);
    peer.join().unwrap();
}
