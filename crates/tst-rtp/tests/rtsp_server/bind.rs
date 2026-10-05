//! RtspServer bind / start / stop / cancel-handle lifecycle integration
//! tests. No RTP/RTCP flow exercised here; the loopback, multicast and
//! client tests cover the actual streaming paths.

use tst_rtp::{RtspServer, RtspServerBuilder, RtspServerError};

#[test]
fn bind_succeeds_on_loopback_port_zero() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    // Before start(), local_addr() is None — the listener hasn't bound yet.
    assert!(server.local_addr().is_none());
}

#[test]
fn start_populates_local_addr() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let addr = server.local_addr().expect("listener bound after start()");
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert!(addr.port() > 0, "kernel-assigned port must be non-zero");
}

#[test]
fn bind_rejects_dns_hostname() {
    let res = RtspServer::bind("rtsp://example.com:8554");
    assert!(
        matches!(res, Err(RtspServerError::UrlParse(_))),
        "DNS host must be rejected at parse time; got {res:?}"
    );
}

#[test]
fn bind_rejects_malformed_url() {
    let res = RtspServer::bind("not-a-url");
    assert!(
        matches!(res, Err(RtspServerError::UrlParse(_))),
        "malformed URL must be rejected; got {res:?}"
    );
}

#[test]
fn start_twice_errors_already_started() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    let e = server.start().unwrap_err();
    assert!(matches!(e, RtspServerError::AlreadyStarted));
}

#[test]
fn stop_before_start_errors_not_started() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let e = server.stop().unwrap_err();
    assert!(matches!(e, RtspServerError::NotStarted));
}

#[test]
fn cancel_handle_obtainable_pre_start() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    // cancel_handle() must work before start() — callers may stash it
    // ahead of time for cross-thread shutdown coordination.
    let h = server.cancel_handle();
    assert!(!h.is_cancelled());
    h.cancel();
    // The same handle observes its own flip.
    assert!(h.is_cancelled());
    // A second handle obtained from the same server observes it too.
    let h2 = server.cancel_handle();
    assert!(h2.is_cancelled());
}

#[test]
fn drop_unstarted_server_does_not_panic() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    drop(server);
}

#[test]
fn drop_started_server_completes_within_runtime_budget() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    server.start().unwrap();
    // Drop's hard-cancel + shutdown_timeout(5s) must complete cleanly.
    drop(server);
}

/// A bind failure (port already taken) must surface from `start()` as a
/// typed error. Pre-fix, `start()` spin-waited 1 s for `local_addr`,
/// then returned `Ok(())` — the caller held a "started" server whose
/// listener task had already died (log-only silent death).
#[test]
fn start_surfaces_bind_addr_in_use() {
    // Occupy a kernel-picked port with a plain std listener, then try to
    // start an RtspServer on the same port.
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("blocker bind");
    let port = blocker.local_addr().expect("blocker addr").port();

    let b = RtspServerBuilder::new(&format!("rtsp://127.0.0.1:{port}")).expect("URL parse");
    let server = b.build().expect("server build");
    let err = server
        .start()
        .expect_err("start() must fail when the port is already bound");
    assert!(
        matches!(err, RtspServerError::BindAddrInUse),
        "expected BindAddrInUse, got {err:?}"
    );
}
