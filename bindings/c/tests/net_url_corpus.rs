//! The C leg of the URL corpus (`crates/tst-integration/tests/fixtures/
//! scenarios/url-corpus/corpus.toml`): every row's URL goes through the C entry point
//! the corpus header names for its scheme and role, and the outcome must match
//! the row's record. C exposes no parsed URL, so only the outcome is compared:
//! a refused row must fail with the expected kind's `TST_E_*` code, an
//! accepted row must not fail with the code of one of the scheme's URL kinds.
//! For the wire schemes (rtsp) a valid row is also connected to a recording
//! peer, and the request heads must show the record's credentials and keys.

#![cfg(all(
    feature = "srt",
    feature = "rtp",
    feature = "udp",
    feature = "tcp",
    feature = "hls",
    feature = "rist"
))]

#[allow(dead_code)]
#[path = "common/mod.rs"]
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tst_integration::scenarios::url::{self, Record, Row};
use tst_srt::{SrtListenerBuilder, SrtSocketBuilder};
use tstrans::config::{tst_sender_config_free, tst_sender_config_new};
use tstrans::error::{tst_clear_last_error, tst_get_last_error};
use tstrans::hls::{
    tst_hls_publisher_builder_free, tst_hls_publisher_builder_from_url,
    tst_hls_publisher_builder_new,
};
use tstrans::receiver::ts_receiver::{tst_receiver_close, tst_receiver_open_listener};
use tstrans::rist::{
    tst_rist_receiver_close, tst_rist_recv_open, tst_rist_sender_close, tst_rist_sender_open,
};
use tstrans::rtp::receiver::{tst_rtp_receiver_close, tst_rtp_recv_open};
use tstrans::rtp::sender::{tst_rtp_sender_close, tst_rtp_sender_open};
use tstrans::sender::ts_sender::{tst_sender_close, tst_sender_open};
use tstrans::tcp::{
    tst_tcp_listener_free, tst_tcp_listener_from_url, tst_tcp_sender_close, tst_tcp_sender_open,
};
use tstrans::udp::{
    tst_udp_receiver_close, tst_udp_recv_open, tst_udp_sender_close, tst_udp_sender_open,
};
use tstrans::{
    tst_rtsp_client_builder_connect, tst_rtsp_client_builder_free,
    tst_rtsp_client_builder_keepalive, tst_rtsp_client_builder_new,
    tst_rtsp_session_teardown_and_free,
};

/// The loopback address of an srt row's URL, when the URL parses. The peer
/// goes up whatever the record says: should C accept a URL the record calls
/// invalid, the open still returns (and the mismatch is reported) instead of
/// waiting in accept forever.
fn peer_addr(row: &Row) -> Option<String> {
    let u = tst_srt::SrtUrl::parse(&row.url).ok()?;
    let host = if u.host.is_empty() {
        "127.0.0.1"
    } else {
        u.host.as_str()
    };
    Some(if host.contains(':') {
        format!("[{host}]:{}", u.port)
    } else {
        format!("{host}:{}", u.port)
    })
}

/// The port written in `url`'s authority, when there is one.
fn url_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://")?.1;
    let authority = &rest[..rest.find(['/', '?']).unwrap_or(rest.len())];
    let hostport = authority.rsplit('@').next()?;
    let port = hostport.rsplit_once(':')?.1;
    port.parse().ok()
}

/// A loopback TCP listener on `port` that accepts and drops every
/// connection, started once per port. A caller dialing a closed loopback port
/// is refused at once on Linux but only after about two seconds of SYN
/// retries on Windows, so the tcp caller rows dial this instead.
fn tcp_peer(port: u16) {
    static BOUND: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();
    let mut bound = BOUND.get_or_init(Default::default).lock().unwrap();
    if !bound.insert(port) {
        return;
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|e| panic!("tcp peer on 127.0.0.1:{port}: {e}"));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            drop(conn);
        }
    });
}

/// The last-error code after an open returned null (or a negative status).
fn last_code() -> Option<i32> {
    Some(unsafe { tst_get_last_error() })
}

/// Run an open that returns a handle, close the handle, and report the
/// outcome: `None` when it returned one.
fn open_close<T>(open: impl FnOnce() -> *mut T, close: impl FnOnce(*mut T)) -> Option<i32> {
    unsafe { tst_clear_last_error() };
    let h = open();
    if h.is_null() {
        last_code()
    } else {
        close(h);
        None
    }
}

/// Open `row`'s URL through `entry`; `None` when the open returned.
fn probe(entry: &str, row: &Row) -> Option<i32> {
    let c_url = CString::new(row.url.as_str()).expect("corpus URLs hold no NUL");
    let u = c_url.as_ptr();
    unsafe {
        match entry {
            // An SRT caller would wait out its connect timeout against a
            // closed port, so the URL's address gets a loopback listener that
            // accepts once; the accept is cancelled as soon as the open
            // returns. The listener takes the URL's passphrase: libsrt
            // rejects a handshake whose encryption the peer lacks only after
            // about a second.
            "tst_sender_open" => {
                let peer = peer_addr(row).map(|addr| {
                    let mut builder = SrtListenerBuilder::new();
                    if let Some(p) = tst_srt::SrtUrl::parse(&row.url)
                        .ok()
                        .and_then(|u| u.config.passphrase)
                    {
                        builder.passphrase(p);
                    }
                    let mut listener = builder
                        .bind(addr)
                        .unwrap_or_else(|e| panic!("{}: peer listener: {e}", row.label()));
                    let cancel = listener.cancel_handle();
                    let accept = std::thread::spawn(move || {
                        let _ = listener.accept_timeout(common::scaled(Duration::from_secs(5)));
                    });
                    (cancel, accept)
                });
                let cfg = tst_sender_config_new();
                let r = open_close(|| tst_sender_open(u, cfg), |h| tst_sender_close(h));
                tst_sender_config_free(cfg);
                if let Some((cancel, accept)) = peer {
                    cancel.cancel();
                    accept.join().expect("peer accept thread");
                }
                r
            }
            // The listener's open blocks in accept, so the URL's address gets
            // a loopback caller; its handshake retries until the bind lands,
            // which takes milliseconds.
            "tst_receiver_open_listener" => {
                let dial = peer_addr(row).map(|addr| {
                    std::thread::spawn(move || {
                        SrtSocketBuilder::new()
                            .connect_timeout(common::scaled(Duration::from_secs(2)))
                            .connect(addr)
                    })
                });
                let r = open_close(|| tst_receiver_open_listener(u), |h| tst_receiver_close(h));
                if let Some(dial) = dial {
                    let _ = dial.join().expect("peer caller thread");
                }
                r
            }
            "tst_rtp_sender_open" => {
                open_close(|| tst_rtp_sender_open(u), |h| tst_rtp_sender_close(h))
            }
            "tst_rtp_recv_open" => {
                open_close(|| tst_rtp_recv_open(u), |h| tst_rtp_receiver_close(h))
            }
            "tst_rtsp_client_builder_new" => open_close(
                || tst_rtsp_client_builder_new(u),
                |h| tst_rtsp_client_builder_free(h),
            ),
            "tst_udp_sender_open" => {
                open_close(|| tst_udp_sender_open(u), |h| tst_udp_sender_close(h))
            }
            "tst_udp_recv_open" => {
                open_close(|| tst_udp_recv_open(u), |h| tst_udp_receiver_close(h))
            }
            "tst_tcp_sender_open" => {
                if let Some(port) = url_port(&row.url) {
                    tcp_peer(port);
                }
                open_close(|| tst_tcp_sender_open(u), |h| tst_tcp_sender_close(h))
            }
            "tst_tcp_listener_from_url" => open_close(
                || tst_tcp_listener_from_url(u),
                |h| tst_tcp_listener_free(h),
            ),
            "tst_hls_publisher_builder_from_url" => {
                tst_clear_last_error();
                let b = tst_hls_publisher_builder_new();
                assert!(!b.is_null(), "tst_hls_publisher_builder_new returned null");
                let rc = tst_hls_publisher_builder_from_url(b, u);
                tst_hls_publisher_builder_free(b);
                (rc != 0).then_some(rc)
            }
            "tst_rist_sender_open" => {
                open_close(|| tst_rist_sender_open(u), |h| tst_rist_sender_close(h))
            }
            "tst_rist_recv_open" => {
                open_close(|| tst_rist_recv_open(u), |h| tst_rist_receiver_close(h))
            }
            other => panic!("{}: the C leg has no entry point {other:?}", row.label()),
        }
    }
}

// ── The RTSP wire probe ─────────────────────────────────────────────────────

/// How the recording peer answers.
#[derive(Clone, Copy)]
enum Peer {
    /// 461 to a UDP SETUP, so the SETUP sequence shows the transport choice.
    RefuseUdp,
    /// 401 to a DESCRIBE without credentials; stops at the first one with.
    Challenge,
}

/// The value of header `name` (case-insensitive) in a request head.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// One MP2T track (static payload type 33).
const MP2T_SDP: &str = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=tst\r\nt=0 0\r\n\
                        a=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

/// A running [`recording_peer`].
struct RecordingPeer {
    port: u16,
    /// Every request head, in arrival order.
    heads: Arc<Mutex<Vec<String>>>,
    /// Set once the peer accepted a connection.
    accepted: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// A loopback RTSP peer on port 0 that records every request head. It serves
/// one client and returns when the client hangs up or `mode` says stop.
fn recording_peer(mode: Peer) -> RecordingPeer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the RTSP peer");
    let port = listener.local_addr().unwrap().port();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&heads);
    let accepted = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&accepted);
    let peer = std::thread::spawn(move || {
        let Ok((sock, _)) = listener.accept() else {
            return;
        };
        seen.store(true, Ordering::SeqCst);
        // The client always sends a request or hangs up, and the peer
        // returns on either; this bounds only a client that does neither.
        sock.set_read_timeout(Some(common::scaled(Duration::from_secs(5))))
            .unwrap();
        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut out = sock;
        loop {
            // The client's requests carry no body: a head ends at the first
            // empty line.
            let mut head = String::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            record.lock().unwrap().push(head.clone());
            let request_line = head.lines().next().unwrap_or_default();
            let method = request_line.split(' ').next().unwrap_or_default();
            let version = request_line.rsplit(' ').next().unwrap_or_default();
            let cseq = header(&head, "cseq").unwrap_or("1");
            let authorized = header(&head, "authorization").is_some();
            let transport = header(&head, "transport").unwrap_or_default();
            if matches!(mode, Peer::Challenge) && method == "DESCRIBE" && authorized {
                return;
            }
            let (status, extra, body) = match method {
                "DESCRIBE" if matches!(mode, Peer::Challenge) => (
                    "401 Unauthorized",
                    "WWW-Authenticate: Basic realm=\"t\"\r\n".to_owned(),
                    "",
                ),
                "OPTIONS" => (
                    "200 OK",
                    "Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n".to_owned(),
                    "",
                ),
                "DESCRIBE" => (
                    "200 OK",
                    format!(
                        "Content-Type: application/sdp\r\nContent-Length: {}\r\n",
                        MP2T_SDP.len()
                    ),
                    MP2T_SDP,
                ),
                "SETUP" if !transport.contains("/TCP") => {
                    ("461 Unsupported Transport", String::new(), "")
                }
                "SETUP" => (
                    "200 OK",
                    format!("Session: DEADBEEF;timeout=60\r\nTransport: {transport}\r\n"),
                    "",
                ),
                _ => ("200 OK", "Session: DEADBEEF\r\n".to_owned(), ""),
            };
            let reply = format!("{version} {status}\r\nCSeq: {cseq}\r\n{extra}\r\n{body}");
            if out.write_all(reply.as_bytes()).is_err() || method == "TEARDOWN" {
                return;
            }
        }
    });
    RecordingPeer {
        port,
        heads,
        accepted,
        thread: peer,
    }
}

/// `url` with its port replaced by `port` (inserted when the URL has none).
fn with_port(url: &str, port: u16) -> String {
    let (scheme, rest) = url.split_once("://").expect("an rtsp URL");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(at) => authority.split_at(at + 1),
        None => ("", authority),
    };
    let host = match hostport.find(']') {
        Some(close) => &hostport[..=close],
        None => hostport.split(':').next().unwrap_or_default(),
    };
    format!("{scheme}://{userinfo}{host}:{port}{tail}")
}

/// Standard base64 decoding, enough for a Basic `Authorization` value.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Connect `url` through the C RTSP client against a peer in `mode`; the
/// request heads the peer saw.
fn rtsp_run(url: &str, mode: Peer) -> Vec<String> {
    let RecordingPeer {
        port,
        heads,
        accepted,
        thread: peer,
    } = recording_peer(mode);
    let c_url = CString::new(with_port(url, port)).expect("corpus URLs hold no NUL");
    unsafe {
        let b = tst_rtsp_client_builder_new(c_url.as_ptr());
        if !b.is_null() {
            // Keepalive off: the recorded heads are only the connect's.
            tst_rtsp_client_builder_keepalive(b, false);
            let s = tst_rtsp_client_builder_connect(b);
            if !s.is_null() {
                tst_rtsp_session_teardown_and_free(s);
            }
        }
    }
    // Unblocks a peer still waiting in accept (the client never dialed).
    // Only then: the peer thread holds the listener until it returns, so this
    // connect finds it listening. Dialing a peer that already returned would
    // hit a closed port, which Windows refuses only after about two seconds.
    if !accepted.load(Ordering::SeqCst) {
        drop(TcpStream::connect(("127.0.0.1", port)));
    }
    peer.join().expect("RTSP peer thread");
    heads.lock().unwrap().clone()
}

/// Project the request heads of the two runs into a record.
fn rtsp_wire_record(plain: &[String], challenged: &[String]) -> Record {
    let mut rec = Record::default();
    let mut keys = BTreeMap::new();
    if let Some(first) = plain.first() {
        let mut parts = first.lines().next().unwrap_or_default().split(' ');
        let uri = parts.nth(1).unwrap_or_default();
        if let Some(version) = parts.next().and_then(|v| v.strip_prefix("RTSP/")) {
            keys.insert("rtsp_version".to_string(), version.to_string());
        }
        // A request URI carries no userinfo; one that leaked would show here.
        let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
        let end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..end];
        let host = match authority.rfind(':') {
            Some(c) if !authority.ends_with(']') => &authority[..c],
            _ => authority,
        };
        rec.host = Some(
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
        );
        rec.path = Some(rest[end..].to_string());
    }
    let setups: Vec<bool> = plain
        .iter()
        .filter(|h| h.starts_with("SETUP "))
        .map(|h| header(h, "transport").unwrap_or_default().contains("/TCP"))
        .collect();
    match setups.as_slice() {
        [true] => drop(keys.insert("transport".into(), "tcp".into())),
        [false, true] => {}
        [false] => drop(keys.insert("transport".into(), "udp".into())),
        other => drop(keys.insert("transport".into(), format!("SETUP sequence {other:?}"))),
    }
    let basic = challenged
        .iter()
        .find_map(|h| header(h, "authorization")?.strip_prefix("Basic "));
    if let Some(b64) = basic {
        let decoded = base64_decode(b64).map(|b| String::from_utf8_lossy(&b).into_owned());
        let decoded = decoded.unwrap_or_else(|| format!("<bad base64 {b64}>"));
        // The client sends credentials only with a password (an empty one
        // counts), so a Basic header at all means one was set.
        let user = decoded.split_once(':').map_or(decoded.as_str(), |(u, _)| u);
        rec.username = Some(user.to_string());
        rec.password_set = Some(true);
    }
    rec.keys = Some(keys);
    rec
}

/// For a wire row: run both peers and compare the declared fields.
fn rtsp_wire_mismatches(row: &Row, fields: &[String]) -> Vec<String> {
    let observed = rtsp_wire_record(
        &rtsp_run(&row.url, Peer::RefuseUdp),
        &rtsp_run(&row.url, Peer::Challenge),
    );
    fields
        .iter()
        .filter_map(|f| {
            let (want, got) = (row.wire_expect(f), url::field(&observed, f));
            (want != got).then(|| {
                format!(
                    "{} on the wire: {f} expected {want:?}, observed {got:?}",
                    row.label()
                )
            })
        })
        .collect()
}

/// The scheme families the C leg opens; each has its own test (and so its
/// own time budget), and a failure names the family.
const FAMILIES: [&str; 7] = ["srt", "rtp", "rtsp", "udp", "tcp", "hls", "rist"];

/// Run every row of `family` through its C entry point and assert the
/// outcomes; print the family's elapsed time and its slowest rows.
fn run_family(family: &str) {
    let started = Instant::now();
    let corpus = url::load();
    let rows: Vec<&Row> = corpus.url.iter().filter(|r| r.family() == family).collect();
    assert!(!rows.is_empty(), "the corpus has no {family} rows");
    let mut mismatches = Vec::new();
    let mut timings = Vec::new();
    let (mut absent, mut wire_rows) = (0, 0);
    for row in &rows {
        let (entry, _probe) = corpus.entry("c", row);
        if entry == "absent" {
            absent += 1;
            continue;
        }
        let row_started = Instant::now();
        let observed = probe(entry, row);
        if let Err(why) = url::check_probe_code(&corpus, row, observed) {
            mismatches.push(format!(
                "{} via {entry}: {why}: {}",
                row.label(),
                common::last_error_msg()
            ));
        }
        if let Some(fields) = corpus.wire_fields("c", row)
            && !row.expect.is_error()
        {
            wire_rows += 1;
            mismatches.extend(rtsp_wire_mismatches(row, &fields));
        }
        timings.push((row_started.elapsed(), row.label()));
    }
    timings.sort_by(|a, b| b.0.cmp(&a.0));
    let slowest: Vec<String> = timings
        .iter()
        .take(3)
        .map(|(d, l)| format!("{d:?} {l}"))
        .collect();
    eprintln!(
        "url corpus C leg, {family}: {} rows ({absent} absent) in {:?}; slowest: {}",
        rows.len(),
        started.elapsed(),
        slowest.join(" | ")
    );
    if corpus.legs["c"][family].contains_key("wire_schemes") {
        assert!(wire_rows > 0, "no {family} row reached the wire probe");
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} {family} rows ({absent} have no C entry point) differ through C:\n{}",
        mismatches.len(),
        rows.len(),
        mismatches.join("\n")
    );
}

#[test]
fn url_corpus_srt_rows_match_through_c() {
    run_family("srt");
}

#[test]
fn url_corpus_rtp_rows_match_through_c() {
    run_family("rtp");
}

#[test]
fn url_corpus_rtsp_rows_match_through_c() {
    run_family("rtsp");
}

#[test]
fn url_corpus_udp_rows_match_through_c() {
    run_family("udp");
}

#[test]
fn url_corpus_tcp_rows_match_through_c() {
    run_family("tcp");
}

#[test]
fn url_corpus_hls_rows_match_through_c() {
    run_family("hls");
}

#[test]
fn url_corpus_rist_rows_match_through_c() {
    run_family("rist");
}

/// The family tests together cover the corpus: every family with a C entry
/// point has one, and every other row is declared absent for C.
#[test]
fn url_corpus_every_c_row_has_a_family_test() {
    let corpus = url::load();
    let mut uncovered = Vec::new();
    for row in &corpus.url {
        let (entry, _probe) = corpus.entry("c", row);
        if entry != "absent" && !FAMILIES.contains(&row.family()) {
            uncovered.push(row.label());
        }
    }
    for family in FAMILIES {
        if !corpus.url.iter().any(|r| r.family() == family) {
            uncovered.push(format!("{family}: a family test with no rows"));
        }
    }
    assert!(
        uncovered.is_empty(),
        "URL corpus rows no C family test runs:\n{}",
        uncovered.join("\n")
    );
}
