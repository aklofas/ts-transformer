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

/// Every way a bind URL can carry user info: a user and a password, a user
/// alone, an empty user with a password, empty user info, an empty pair, and
/// a user with an empty password; on both schemes.
const USERINFO_BIND_URLS: &[&str] = &[
    "rtsp://admin:secret@127.0.0.1:0",
    "rtsp://admin@127.0.0.1:0",
    "rtsp://:secret@127.0.0.1:0",
    "rtsp://@127.0.0.1:0",
    "rtsp://:@127.0.0.1:0",
    "rtsp://admin:@127.0.0.1:0",
    "rtsps://admin:secret@127.0.0.1:0",
    "rtsps://admin@127.0.0.1:0",
    "rtsps://:secret@127.0.0.1:0",
    "rtsps://@127.0.0.1:0",
    "rtsps://:@127.0.0.1:0",
    "rtsps://admin:@127.0.0.1:0",
];

fn is_invalid_server_config(res: Result<RtspServer, RtspServerError>) -> bool {
    matches!(res, Err(RtspServerError::InvalidServerConfig { .. }))
}

/// A server never takes its credential from the bind URL: the URL cannot say
/// a scheme or a realm, so user info there would be silently dropped and the
/// server would start open. It is refused at `build()`, through every door,
/// with or without a credential configured separately.
#[test]
fn a_bind_url_with_userinfo_is_refused() {
    for &url in USERINFO_BIND_URLS {
        for with_auth in [false, true] {
            let configure = |b: &mut RtspServerBuilder| {
                if with_auth {
                    b.auth_basic(
                        "realm",
                        "admin",
                        secrecy::SecretString::new("secret".into()),
                    );
                }
            };
            let mut b = RtspServerBuilder::new(url).expect("URL parse");
            configure(&mut b);
            assert!(
                is_invalid_server_config(b.build()),
                "new({url}), auth {with_auth}"
            );

            let mut b = RtspServerBuilder::with_url(tst_rtp::RtspUrl::parse(url).expect("parse"));
            configure(&mut b);
            assert!(
                is_invalid_server_config(b.build()),
                "with_url({url}), auth {with_auth}"
            );
        }
        assert!(
            is_invalid_server_config(RtspServer::bind(url)),
            "bind({url})"
        );
        assert!(
            is_invalid_server_config(RtspServer::bind_with(
                tst_rtp::RtspUrl::parse(url).expect("parse")
            )),
            "bind_with({url})"
        );
    }
}

/// The refusal says what is wrong without repeating the credential.
#[test]
fn the_bind_url_refusal_names_no_credential() {
    let e = RtspServer::bind("rtsp://admin:secret@127.0.0.1:0").expect_err("refused");
    for text in [e.to_string(), format!("{e:?}")] {
        assert!(text.contains("bind URL"), "{text}");
        assert!(!text.contains("admin"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }
}

/// Basic sends `user:password` and the server splits at the first colon, so a
/// Basic user name with a colon could never authenticate.
#[test]
fn a_basic_user_name_with_a_colon_is_refused_at_build() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    b.auth_basic(
        "realm",
        "ad:min",
        secrecy::SecretString::new("secret".into()),
    );
    let e = b.build().expect_err("refused");
    assert!(
        matches!(e, RtspServerError::InvalidServerConfig { .. }),
        "{e:?}"
    );
    assert!(!e.to_string().contains("ad:min"), "{e}");
    assert!(!e.to_string().contains("secret"), "{e}");
}

/// Digest carries the user name as a quoted parameter, so a colon in it works.
#[test]
fn a_digest_user_name_with_a_colon_is_accepted() {
    for sha256 in [false, true] {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
        let password = secrecy::SecretString::new("secret".into());
        if sha256 {
            b.auth_digest_sha256("realm", "ad:min", password);
        } else {
            b.auth_digest_md5("realm", "ad:min", password);
        }
        assert!(b.build().is_ok(), "sha256 {sha256}");
    }
}
