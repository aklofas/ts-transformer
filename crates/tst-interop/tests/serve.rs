//! Self-tests for `serve::run_hls`/`serve::run_rtsp`: bind an ephemeral
//! HLS/RTSP serve, pull it back with a real client (raw HTTP for HLS,
//! this crate's own `tst_rtp::RtspClient` for RTSP), and check the
//! result against the `baseline` profile's invariants.
//!
//! This binary's name (`serve`) does not match any of
//! `.config/nextest.toml`'s `binary(...)` filter entries, so neither
//! test is placed in the capped `network` test-group by binary name.
//! Both test *names* below deliberately contain `round_trip` (matching
//! `test(round_trip)`), and the RTSP test additionally contains `rtsp`
//! (matching `test(rtsp)`) — same naming convention `tests/loopback.rs`
//! uses for its own network cells. Both therefore DO land in the
//! `network` group (serialized against this crate's other network
//! tests) with the 10s-period/2-terminate (20s hard kill; 40s on
//! Windows via the platform override) budget, not the default profile's
//! 30s/2 (60s) global safety net. Both cells complete in a few wall-clock
//! seconds in the happy path (bounded retries + a short profile window),
//! comfortably inside either budget.
//!
//! Deterministic-test policy (mirrors `tests/loopback.rs`):
//! - Ports are ephemeral, discovered via a throwaway TCP bind (HLS/RTSP
//!   are both TCP-based, unlike `loopback.rs`'s UDP-probed ports).
//! - No fixed sleeps as synchronization: every wait is either a bounded
//!   retry loop or a bounded thread-join.
//! - The `serve::run_hls`/`run_rtsp` producer threads are NOT joined on
//!   the happy path — they linger for `serve::LINGER` (10s) after
//!   finishing their push, which would blow past this file's own nextest
//!   budget if we waited for them. `handle.is_finished()` is checked
//!   after the test's own verification completes so an early failure
//!   (e.g. a bind error) still surfaces a clear panic message; the
//!   happy-path thread is left to finish lingering and exit on its own
//!   (nextest reaps the whole test process's threads at test-process
//!   exit either way).
//!
//! # A real gap this file's RTSP test had to work around
//!
//! `tst_rtp::RtpRecvTransport` (returned by `RtspSession::
//! into_recv_transport()`) is NOT one of `transport.rs`'s wrapped
//! schemes (udp/tcp/rist/srt) — it's driven directly, raw. Unlike every
//! scheme `transport.rs` wraps (see that module's "Bounded receive"
//! doc section), `RtpRecvTransport::recv_bytes` never returns
//! `TransportError::Backpressure` on its own: it blocks internally,
//! polling only its own `cancel_handle()` between retries, until either
//! data arrives or that handle is fired — from ANY thread. Once
//! `serve::run_rtsp`'s producer stops pushing (its `seconds` window
//! ends), no more datagrams ever arrive, so whatever `recv_bytes` call
//! is in flight blocks forever.
//!
//! `recv::recv_over_transport`'s own deadline logic (`NO_DATA_TIMEOUT`/
//! `POST_START_GRACE`) is a SINGLE-THREADED check-then-recv loop — it
//! computes the right deadline, but by the time it would fire `rx.
//! close()`, the same thread is already stuck inside that same final
//! blocking `recv_event()` call, so the close() line never runs. This
//! is invisible for every OTHER transport this crate drives
//! `recv_over_transport` against, because `transport.rs` wraps all of
//! them (`BoundedUdpRecv`, SRT's `SRTO_RCVTIMEO`, RIST's native poll) so
//! `recv_bytes` always returns control periodically on its own — but
//! this file is the first place this crate drives `recv_over_transport`
//! against a raw, unwrapped transport (`RtpRecvTransport`) at all.
//!
//! [`rtsp_serve_round_trip_via_own_client`] works around this with an
//! external-cancel watchdog: obtain the transport's `cancel_handle()`
//! BEFORE handing it to `recv_over_transport` (run on its own thread),
//! then externally fire that handle from a bounded watchdog if the
//! worker hasn't finished on its own — the watchdog's own bound is what
//! keeps this test deterministic, not `recv_over_transport`'s internal
//! (here, ineffective) deadline.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, TransportCancel};
use tst_interop::{profiles, recv, serve, verify};
use tst_rtp::RtspClient;

/// Seconds of synthetic traffic per cell. Long enough to clear the
/// 70%-of-nominal count floors (`verify::NOMINAL_COUNT_SLACK`) with
/// margin, short enough to keep both cells comfortably inside the
/// network test-group's 20s kill (see the module doc).
const SECONDS: f64 = 3.0;

/// Poll `handle.is_finished()` until it's done or `timeout` elapses —
/// the bounded-wait counterpart to a bare `.join()`, mirroring
/// `tests/loopback.rs::join_with_timeout`.
fn join_with_timeout<T>(handle: thread::JoinHandle<T>, timeout: Duration) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if handle.is_finished() {
            return handle.join().expect("spawned thread panicked");
        }
        assert!(
            Instant::now() < deadline,
            "thread did not finish within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Wait up to `timeout` for `handle` to finish on its own; if it
/// hasn't, fire `cancel` (see the module doc's "real gap" section for
/// why this is needed against a raw `RtpRecvTransport`) and join with
/// one more bounded `grace` period.
fn join_with_external_cancel<T>(
    handle: thread::JoinHandle<T>,
    cancel: Option<Arc<dyn TransportCancel + Send + Sync>>,
    timeout: Duration,
    grace: Duration,
) -> T {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            if let Some(c) = &cancel {
                c.cancel();
            }
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    join_with_timeout(handle, grace)
}

/// Ask the OS for an unused TCP port via a throwaway bind. HLS and RTSP
/// are both TCP-based (HTTP / RTSP control), unlike `loopback.rs`'s
/// UDP-probed ports. Small TOCTOU race between this probe's drop and the
/// real bind that follows — the same accepted trade-off `loopback.rs`'s
/// `free_port` documents for its own analogous pick.
fn free_tcp_port() -> u16 {
    let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe port");
    probe.local_addr().expect("read probe local_addr").port()
}

/// Bounded wait for `addr` to start accepting TCP connections —
/// deterministic stand-in for "the server has bound its listener yet".
fn wait_for_accept(addr: SocketAddr, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "server at {addr} never started accepting within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Perform a raw HTTP/1.1 GET against `addr` and return `(status_line,
/// body_bytes)`. Segment responses are binary MPEG-TS, so — unlike
/// `tst-hls`'s own `tests/http_hardening.rs::http_get` helper, which
/// reads the whole response via `read_to_string` (fine for its
/// text-only playlist assertions) — this reads raw bytes
/// (`read_to_end`) and splits the header/body manually at the first
/// blank line, so segment bytes survive untouched for the byte-level
/// `verify_file` check below.
fn http_get_raw(addr: SocketAddr, path: &str) -> (String, Vec<u8>) {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    sock.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .expect("write request");
    let mut resp = Vec::new();
    sock.read_to_end(&mut resp).expect("read response");
    let sep = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no header/body separator in HTTP response");
    let header = String::from_utf8_lossy(&resp[..sep]).into_owned();
    let body = resp[sep + 4..].to_vec();
    (header, body)
}

/// HLS: serve `baseline` on an ephemeral port, poll `/playlist.m3u8`
/// until it reaches its terminal (`#EXT-X-ENDLIST`) form (written by
/// `finish_serving` — see `serve.rs`'s module doc), fetch every segment
/// it lists in playlist order, concatenate the raw bytes, and
/// `verify_file` the result against `baseline`'s invariants.
#[test]
fn hls_serve_round_trip_matches_baseline() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let bind_addr: SocketAddr = format!("127.0.0.1:{}", free_tcp_port())
        .parse()
        .expect("bind_addr must parse");

    let handle = thread::spawn(move || serve::run_hls(profile, bind_addr, SECONDS));

    wait_for_accept(bind_addr, Duration::from_secs(5));

    // Poll until the playlist reaches its terminal ENDLIST form — an
    // early GET may see a live, still-growing playlist (finish_serving
    // only runs after the full SECONDS window has been pushed).
    let deadline = Instant::now() + Duration::from_secs(SECONDS as u64 + 10);
    let playlist = loop {
        let (header, body) = http_get_raw(bind_addr, "/playlist.m3u8");
        assert!(
            header.starts_with("HTTP/1.1 200"),
            "playlist GET failed: {header}"
        );
        let text = String::from_utf8(body).expect("playlist response must be UTF-8 text");
        if text.contains("#EXT-X-ENDLIST") {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "playlist never reached #EXT-X-ENDLIST within the deadline"
        );
        thread::sleep(Duration::from_millis(100));
    };

    // Fetch every segment_*.ts the playlist lists, in playlist order,
    // and concatenate the raw bytes into one contiguous capture.
    let mut ts_bytes = Vec::new();
    for line in playlist.lines() {
        if line.ends_with(".ts") {
            let (header, body) = http_get_raw(bind_addr, &format!("/{line}"));
            assert!(
                header.starts_with("HTTP/1.1 200"),
                "segment GET for {line} failed: {header}"
            );
            ts_bytes.extend_from_slice(&body);
        }
    }
    assert!(!ts_bytes.is_empty(), "no segments listed in the playlist");

    let path = std::env::temp_dir().join(format!(
        "tst-interop-hls-serve-{}-{}.ts",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos()
    ));
    std::fs::write(&path, &ts_bytes).expect("write fetched TS bytes to a temp file");
    let result = verify::verify_file(&path, profile, SECONDS);
    let _ = std::fs::remove_file(&path);
    let report = result.expect("verify_file must succeed reading the fetched capture");

    assert!(
        report.pass,
        "HLS-served capture failed verification: {:?}",
        report.failures
    );

    // See the module doc: don't wait for the lingering server thread —
    // only surface its result if it already finished (a fast failure).
    if handle.is_finished() {
        handle
            .join()
            .expect("serve thread panicked")
            .expect("run_hls must succeed");
    }
}

/// Connect our own `RtspClient` against the server `serve::run_rtsp`
/// bound, negotiate MP2T, and PLAY — returning the client (must stay
/// alive: dropping it sends TEARDOWN) and the resulting recv transport.
fn connect_and_play(
    bind_addr: SocketAddr,
    mount: &str,
) -> Result<(RtspClient, Box<dyn RecvTransport>), String> {
    let url = format!("rtsp://{bind_addr}{mount}");
    let mut client = RtspClient::connect(&url).map_err(|e| format!("connect: {e}"))?;
    client.options().map_err(|e| format!("options: {e}"))?;
    let sdp = client.describe().map_err(|e| format!("describe: {e}"))?;
    let session = client
        .setup_mp2t_auto(&sdp)
        .map_err(|e| format!("setup_mp2t_auto: {e}"))?;
    let recv = session.into_recv_transport();
    client.play().map_err(|e| format!("play: {e}"))?;
    Ok((client, Box::new(recv)))
}

/// RTSP: serve `baseline` on an ephemeral port, consume it with our own
/// `RtspClient`, feed the received TS bytes through
/// `recv::recv_over_transport` (the same Tally-driving loop `recv.rs`
/// uses for every other transport), and check the result against
/// `baseline`'s invariants. See the module doc's "real gap" section for
/// why this needs an external-cancel watchdog rather than a bare call.
#[test]
fn rtsp_serve_round_trip_via_own_client() {
    let profile = profiles::by_name("baseline").expect("baseline profile must exist");
    let bind_addr: SocketAddr = format!("127.0.0.1:{}", free_tcp_port())
        .parse()
        .expect("bind_addr must parse");
    const MOUNT: &str = "/live";

    let handle = thread::spawn(move || serve::run_rtsp(profile, bind_addr, MOUNT, SECONDS));

    wait_for_accept(bind_addr, Duration::from_secs(5));

    // The producer thread's mount registration can still be racing this
    // thread's connect attempt even after the TCP listener itself is
    // accepting (accept happens before add_mount's mutex state settles
    // in the very first connection); retry on a bounded budget, the same
    // pattern `tests/loopback.rs::send_with_retry` uses for its SRT cell.
    let deadline = Instant::now() + Duration::from_secs(5);
    let (client, recv_transport) = loop {
        match connect_and_play(bind_addr, MOUNT) {
            Ok(pair) => break pair,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "RTSP client setup kept failing until the retry budget ran out: {e}"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    };

    // Obtain the cancel handle BEFORE moving recv_transport into the
    // worker thread — see the module doc's "real gap" section.
    let cancel = recv_transport.cancel_handle();
    let recv_handle = thread::spawn(move || {
        recv::recv_over_transport(recv_transport, profile, SECONDS, false, false, None)
    });
    let report = join_with_external_cancel(
        recv_handle,
        cancel,
        Duration::from_secs(SECONDS as u64 + 5),
        Duration::from_secs(5),
    )
    .expect("recv_over_transport must succeed");
    drop(client); // best-effort TEARDOWN, same as recv_rtsp_camera.rs's example

    assert!(
        report.pass,
        "RTSP-served capture failed verification: {:?}",
        report.failures
    );

    if handle.is_finished() {
        handle
            .join()
            .expect("serve thread panicked")
            .expect("run_rtsp must succeed");
    }
}
