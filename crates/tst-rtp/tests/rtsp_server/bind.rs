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

/// Bind URLs that carry a path: one segment, several, a doubled slash, and
/// a `#` (the URL type has no fragment, so it stays in the path).
const PATH_BIND_URLS: &[&str] = &[
    "rtsp://127.0.0.1:0/live",
    "rtsps://127.0.0.1:0/live",
    "rtsp://127.0.0.1:0/a/b",
    "rtsp://127.0.0.1:0//",
    "rtsp://127.0.0.1:0/#frag",
    "rtsp://[::1]:0/live",
];

/// Bind URLs that carry a query: each key a client URL knows, a value that
/// only restates the default, and an empty query, with and without a slash.
const QUERY_BIND_URLS: &[&str] = &[
    "rtsp://127.0.0.1:0?transport=tcp",
    "rtsp://127.0.0.1:0?transport=udp",
    "rtsps://127.0.0.1:0?transport=tcp",
    "rtsps://127.0.0.1:0?transport=udp",
    "rtsp://127.0.0.1:0?rtsp_version=1.0",
    "rtsp://127.0.0.1:0?tcp_keepalive=5",
    "rtsp://127.0.0.1:0?recv_timeout=100",
    "rtsp://127.0.0.1:0?",
    "rtsp://127.0.0.1:0/?",
    "rtsp://127.0.0.1:0/?transport=tcp",
];

/// The detail of the refusal `build()` gives for `url`, through each of the
/// four ways a server is built from a URL; they must agree.
fn bind_url_refusal(url: &str) -> String {
    let detail = |res: Result<RtspServer, RtspServerError>, door: &str| match res {
        Err(RtspServerError::InvalidServerConfig { detail }) => detail,
        Err(e) => panic!("{door}({url}): expected InvalidServerConfig, got {e:?}"),
        Ok(_) => panic!("{door}({url}): the server was built"),
    };
    let parsed = || tst_rtp::RtspUrl::parse(url).expect("parse");
    let through_new = detail(
        RtspServerBuilder::new(url).expect("URL parse").build(),
        "new",
    );
    for (other, door) in [
        (RtspServerBuilder::with_url(parsed()).build(), "with_url"),
        (RtspServer::bind(url), "bind"),
        (RtspServer::bind_with(parsed()), "bind_with"),
    ] {
        assert_eq!(detail(other, door), through_new, "{door}({url})");
    }
    through_new
}

/// A bind URL says where to listen and nothing else: a path would read like
/// a mount and is none.
#[test]
fn a_bind_url_with_a_path_is_refused() {
    for &url in PATH_BIND_URLS {
        let detail = bind_url_refusal(url);
        assert!(detail.contains("bind URL"), "{url}: {detail}");
        assert!(detail.contains("path"), "{url}: {detail}");
    }
}

/// The query keys are a client's and none configures a server, so any query
/// is refused, also an empty one and one that restates a default; the text
/// points at the setting that does decide UDP media.
#[test]
fn a_bind_url_with_a_query_is_refused() {
    for &url in QUERY_BIND_URLS {
        let detail = bind_url_refusal(url);
        assert!(detail.contains("bind URL"), "{url}: {detail}");
        assert!(detail.contains("query"), "{url}: {detail}");
        assert!(detail.contains("allow_udp_media"), "{url}: {detail}");
    }
}

/// No path and a bare `/` are the same bind URL, on both schemes.
#[test]
fn a_bind_url_without_a_path_or_with_a_bare_slash_builds() {
    for url in [
        "rtsp://127.0.0.1:0",
        "rtsp://127.0.0.1:0/",
        "rtsps://127.0.0.1:0",
        "rtsps://127.0.0.1:0/",
        "rtsp://[::1]:0/",
    ] {
        RtspServerBuilder::new(url)
            .expect("URL parse")
            .build()
            .unwrap_or_else(|e| panic!("{url}: {e:?}"));
        RtspServer::bind_with(tst_rtp::RtspUrl::parse(url).expect("parse"))
            .unwrap_or_else(|e| panic!("{url}: {e:?}"));
    }
}

/// What the URL parse refuses stays a parse error from `new`, before any
/// builder exists: a key no RTSP URL knows, a known key with a bad value,
/// and a `#` straight after the port.
#[test]
fn a_bind_url_the_parse_refuses_is_a_url_parse_error() {
    use tst_rtp::RtpUrlError;
    assert!(matches!(
        RtspServerBuilder::new("rtsp://127.0.0.1:0?mount=live").err(),
        Some(RtspServerError::UrlParse(
            RtpUrlError::UnknownQueryKey { .. }
        ))
    ));
    assert!(matches!(
        RtspServerBuilder::new("rtsp://127.0.0.1:0?transport=quic").err(),
        Some(RtspServerError::UrlParse(RtpUrlError::BadQuery { .. }))
    ));
    assert!(matches!(
        RtspServerBuilder::new("rtsp://127.0.0.1:0#frag").err(),
        Some(RtspServerError::UrlParse(RtpUrlError::Syntax(_)))
    ));
}

/// User info is refused first, so a URL that also carries a path and a query
/// still names no credential.
#[test]
fn a_bind_url_with_userinfo_and_a_path_names_no_credential() {
    let e = RtspServer::bind("rtsp://admin:secret@127.0.0.1:0/live?transport=tcp")
        .expect_err("refused");
    assert!(
        matches!(e, RtspServerError::InvalidServerConfig { .. }),
        "{e:?}"
    );
    for text in [e.to_string(), format!("{e:?}")] {
        assert!(!text.contains("admin"), "{text}");
        assert!(!text.contains("secret"), "{text}");
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
