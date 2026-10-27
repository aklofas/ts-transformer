//! HTTP hardening integration tests.
//!
//! Verifies that the serve-from-known-set security model (CWE-22 path-traversal
//! class) is structurally enforced:
//!
//! - Only filenames the segmenter itself created and still tracks (history ∪
//!   grace) resolve to bytes; every other name → 404.
//! - The `route()` dispatcher rejects non-GET methods and multi-component paths.
//! - Basic-auth is checked before any routing (all routes return 401 when
//!   credentials are absent).

#![cfg(feature = "serve")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_test_helpers::timing::scaled;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Perform a raw HTTP/1.1 GET against `addr` and return the full response as a
/// string (headers + body).
fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    http_raw(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"),
    )
}

/// Send a raw HTTP request string and return the full response.
fn http_raw(addr: std::net::SocketAddr, request: &str) -> String {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    sock.write_all(request.as_bytes()).expect("write request");
    let mut resp = String::new();
    let _ = sock.read_to_string(&mut resp);
    resp
}

/// Assert the first line of an HTTP response starts with the expected status
/// code, e.g. "HTTP/1.1 404 ".
trait StartsWithStatus {
    fn starts_with_status(&self, code: u16) -> bool;
}

impl StartsWithStatus for str {
    fn starts_with_status(&self, code: u16) -> bool {
        let expected = format!("HTTP/1.1 {code} ");
        self.starts_with(&expected)
    }
}

fn tmpdir(label: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "hls-hardening-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Build a publisher with one committed segment, return (publisher, addr).
/// The caller owns the publisher and should call `.finish()` after the test.
fn publisher_with_one_segment(label: &str) -> (tst_hls::HlsPublisher, std::net::SocketAddr) {
    let dir = tmpdir(label);
    let publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .playlist_window(6)
        .mode(HlsMode::Event)
        .build()
        .unwrap();
    let addr = publisher.local_addr().unwrap();

    // Push one segment so /segment_00000.ts exists and is in history.
    use tst_core::publisher::Publisher;
    let mut p = publisher;
    p.push_ts(&[0x47u8; 376]).unwrap();
    p.cut_segment().unwrap();

    (p, addr)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every traversal path and unknown-name lookup must return 404; the real
/// committed segment must return 200.
#[test]
fn traversal_and_unknown_names_404() {
    let (p, addr) = publisher_with_one_segment("traversal");

    let traversal_paths = [
        "/../Cargo.toml",
        "/..%2F..%2Fetc%2Fpasswd",
        "/segment_../secret.ts",
        "/segment_99999.ts", // well-formed name but not in the known set
        "/playlist.m3u8/../x.ts",
        "//etc/passwd",
        "/segment_00000.ts%00.ts",
    ];

    for path in traversal_paths {
        let resp = http_get(addr, path);
        assert!(
            resp.starts_with_status(404),
            "path {path:?} must 404, got:\n{resp}"
        );
    }

    // The real, committed segment must still serve.
    let resp = http_get(addr, "/segment_00000.ts");
    assert!(
        resp.starts_with_status(200),
        "/segment_00000.ts must 200, got:\n{resp}"
    );

    p.finish().unwrap();
}

/// Non-GET methods are rejected (404 or 405).
#[test]
fn non_get_methods_rejected() {
    let (p, addr) = publisher_with_one_segment("non-get");

    let resp = http_raw(
        addr,
        "POST /playlist.m3u8 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    );
    assert!(
        resp.starts_with_status(404) || resp.starts_with_status(405),
        "POST must return 404 or 405, got:\n{resp}"
    );

    p.finish().unwrap();
}

// ---------------------------------------------------------------------------
// Finished state + finish_serving
// ---------------------------------------------------------------------------

/// VOD publisher with 2 segments: after `finish_serving()` the served playlist
/// must carry `#EXT-X-PLAYLIST-TYPE:VOD` and `#EXT-X-ENDLIST`, segments must
/// be reachable, and the server must stop after `shutdown()`.
#[test]
fn vod_served_after_finish_until_handle_drop() {
    let dir = tmpdir("vod-finish");
    let mut p = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(std::time::Duration::from_secs(10))
        .mode(HlsMode::Vod)
        .build()
        .unwrap();

    // Push two segments.
    p.push_ts(&[0x47u8; 376]).unwrap();
    p.cut_segment().unwrap();
    p.push_ts(&[0x47u8; 376]).unwrap();
    p.cut_segment().unwrap();

    let handle = p.finish_serving().unwrap();
    let addr = handle.local_addr();

    // Playlist must be terminal.
    let pl = http_get(addr, "/playlist.m3u8");
    assert!(pl.contains("200 OK"), "playlist must return 200:\n{pl}");
    assert!(
        pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"),
        "must carry VOD type:\n{pl}"
    );
    assert!(
        pl.contains("#EXT-X-ENDLIST"),
        "served playlist must be terminal:\n{pl}"
    );

    // Both segments must be reachable.
    let seg0 = http_get(addr, "/segment_00000.ts");
    assert!(
        seg0.starts_with_status(200),
        "/segment_00000.ts must 200 after finish_serving:\n{seg0}"
    );
    let seg1 = http_get(addr, "/segment_00001.ts");
    assert!(
        seg1.starts_with_status(200),
        "/segment_00001.ts must 200 after finish_serving:\n{seg1}"
    );

    // Shutdown — server must stop accepting.
    handle.shutdown();
    // Give the runtime a moment to drain.
    std::thread::sleep(std::time::Duration::from_millis(100));
    let still_up = std::net::TcpStream::connect(addr).is_ok();
    assert!(!still_up, "server must stop serving after shutdown");
}

/// EVENT publisher: after `finish_serving()` the playlist carries `#EXT-X-ENDLIST`.
#[test]
fn event_served_after_finish_has_endlist() {
    let dir = tmpdir("event-finish");
    let mut p = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(std::time::Duration::from_secs(10))
        .mode(HlsMode::Event)
        .build()
        .unwrap();

    p.push_ts(&[0x47u8; 376]).unwrap();
    p.cut_segment().unwrap();

    let handle = p.finish_serving().unwrap();
    let addr = handle.local_addr();

    let pl = http_get(addr, "/playlist.m3u8");
    assert!(
        pl.contains("#EXT-X-ENDLIST"),
        "EVENT served playlist must be terminal:\n{pl}"
    );

    handle.shutdown();
}

/// A VOD playlist never changes (RFC 8216 section 4.3.3.5), so before finish
/// it is neither served nor written; finish produces the complete playlist.
#[test]
fn vod_playlist_is_404_until_finish() {
    let dir = tmpdir("vod-404");
    let mut p = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .mode(HlsMode::Vod)
        .build()
        .unwrap();
    let addr = p.local_addr().unwrap();
    for _ in 0..2 {
        p.push_ts(&[0x47u8; 376]).unwrap();
        p.cut_segment().unwrap();
    }

    let pl = http_get(addr, "/playlist.m3u8");
    assert!(pl.contains("404"), "unfinished VOD must 404:\n{pl}");
    assert!(
        !dir.join("playlist.m3u8").exists(),
        "no playlist file before finish"
    );

    let handle = p.finish_serving().unwrap();
    let pl = http_get(handle.local_addr(), "/playlist.m3u8");
    assert!(pl.contains("200 OK"), "finished VOD must 200:\n{pl}");
    assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{pl}");
    assert!(pl.contains("#EXT-X-ENDLIST"), "{pl}");
    assert!(
        dir.join("playlist.m3u8").exists(),
        "file present after finish"
    );
    handle.shutdown();
}

/// A VOD publisher finished without any segment still publishes a complete
/// (empty) playlist, and only at finish.
#[test]
fn vod_with_no_segments_publishes_only_at_finish() {
    let dir = tmpdir("vod-empty");
    let p = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .mode(HlsMode::Vod)
        .build()
        .unwrap();
    let addr = p.local_addr().unwrap();

    let pl = http_get(addr, "/playlist.m3u8");
    assert!(pl.contains("404"), "unfinished VOD must 404:\n{pl}");
    assert!(!dir.join("playlist.m3u8").exists());

    let handle = p.finish_serving().unwrap();
    let pl = http_get(handle.local_addr(), "/playlist.m3u8");
    assert!(pl.contains("200 OK"), "{pl}");
    assert!(pl.contains("#EXT-X-ENDLIST"), "{pl}");
    assert!(!pl.contains("#EXTINF"), "no segment lines:\n{pl}");
    assert!(dir.join("playlist.m3u8").exists());
    handle.shutdown();
}

/// With basic_auth configured, unauthenticated GETs to both /playlist.m3u8
/// and /segment_00000.ts each return 401.
#[test]
fn basic_auth_required_for_all_routes() {
    let dir = tmpdir("auth-all-routes");
    let mut p = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .playlist_window(6)
        .mode(HlsMode::Event)
        .basic_auth("alice", "s3cret")
        .build()
        .unwrap();
    let addr = p.local_addr().unwrap();

    p.push_ts(&[0x47u8; 376]).unwrap();
    p.cut_segment().unwrap();

    for path in ["/playlist.m3u8", "/segment_00000.ts"] {
        let resp = http_get(addr, path);
        assert!(
            resp.starts_with_status(401),
            "unauthenticated GET of {path:?} must return 401, got:\n{resp}"
        );
    }

    p.finish().unwrap();
}

/// `shutdown()` returns within its 2 s bound while a client holds a
/// half-sent request open.
#[test]
fn shutdown_returns_promptly_with_a_slow_client_connected() {
    let dir = tmpdir("slow-client");
    let mut p = HlsPublisherBuilder::new()
        .bind("127.0.0.1:0".parse().unwrap())
        .output_dir(&dir)
        .segment_duration(Duration::from_secs(10))
        .mode(HlsMode::Vod)
        .build()
        .unwrap();
    p.push_ts(&[0x47u8; 376]).unwrap();
    p.cut_segment().unwrap();
    let handle = p.finish_serving().unwrap();

    let mut slow = TcpStream::connect(handle.local_addr()).unwrap();
    slow.write_all(b"GET /playlist.m3u8 HTTP/1.1\r\nHost: x\r\n")
        .unwrap(); // no blank line
    std::thread::sleep(Duration::from_millis(100));

    let t0 = std::time::Instant::now();
    handle.shutdown();
    assert!(
        t0.elapsed() < scaled(Duration::from_secs(2)),
        "shutdown took {:?}",
        t0.elapsed()
    );
}
