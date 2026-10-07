//! Verifies the rtsp-keepalive background thread emits OPTIONS pings on
//! the control TCP within `session_timeout / 2` (or the explicit
//! override), and that drop-cleanup joins the thread cleanly.
//!
//! Uses a hand-rolled TCP `accept()` loop instead of a full RTSP server
//! — we only care about observing the wire format the keepalive emits.

use std::io::{Read, Write};
use std::net::TcpListener;

#[test]
fn keepalive_thread_pings_within_session_timeout() {
    // Bind a loopback listener and capture the port for the URL.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Background server: accept one connection, expect the keepalive
    // thread's OPTIONS ping at CSeq 1000001 within ~10 s, then reply
    // 200 OK so the keepalive's read-loop sees a clean close on drop.
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

/// In non-pump mode (UDP transport / pre-SETUP) nothing drains the
/// control TCP between requests, so a keepalive ping's 200 OK can sit in
/// the socket buffer ahead of the next real exchange. The read path must
/// consume it by its CSeq (≥ 1_000_000 = the keepalive range) and keep
/// reading — returning it would misattribute it as the response to
/// whatever request the caller just sent. Here the stale keepalive 200
/// carries no `Public:` header, so a misattributing `options()` would return an empty
/// method list from the wrong response.
#[test]
fn stale_keepalive_response_not_misattributed_in_non_pump_mode() {
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
