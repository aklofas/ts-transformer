//! Server-side Digest auth integration tests.
//!
//! Mirror of `auth_basic.rs` but exercises
//! `RtspServerBuilder::auth_digest_md5` and `auth_digest_sha256`.

use crate::fixtures::raw_rtsp::make_muxer_cfg;
use secrecy::SecretString;
use tst_rtp::{RtspClient, RtspError, RtspServer, RtspServerBuilder};

fn server_with_digest_md5() -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    b.auth_digest_md5("test-realm", "admin", SecretString::new("secret".into()));
    let server = b.build().expect("server build");
    let _mount = server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    server
}

fn server_with_digest_sha256() -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    b.auth_digest_sha256("test-realm", "admin", SecretString::new("secret".into()));
    let server = b.build().expect("server build");
    let _mount = server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    server
}

#[test]
fn digest_md5_no_credentials_returns_401() {
    let server = server_with_digest_md5();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS without auth");
    let e = client
        .describe()
        .expect_err("DESCRIBE without creds must 401");
    assert!(matches!(e, RtspError::AuthFailed), "got: {e:?}");
    server.stop().ok();
}

#[test]
fn digest_md5_valid_credentials_succeed() {
    let server = server_with_digest_md5();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:secret@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS");
    let sdp = client.describe().expect("DESCRIBE with creds");
    assert!(!sdp.media.is_empty(), "SDP should advertise media");
    server.stop().ok();
}

#[test]
fn digest_sha256_valid_credentials_succeed() {
    let server = server_with_digest_sha256();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:secret@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS");
    let sdp = client.describe().expect("DESCRIBE with creds");
    assert!(!sdp.media.is_empty(), "SDP should advertise media");
    server.stop().ok();
}

/// A username containing `"` must still authenticate end-to-end. The URL
/// carries the percent-encoded form (`a%22b`); `RtspUrl::parse`
/// percent-decodes it to the literal `a"b`, the client's
/// `Authorization:` header backslash-escapes it to `username="a\"b"`
/// (`escape_quoted_string`), and the server must UN-escape it back to
/// `a"b` before comparing against the configured username
/// (`parse_kv_pairs` / `unescape_quoted_string`) — before that server-side
/// fix, the stored value stayed the literal `a\"b` and this failed.
#[test]
fn digest_md5_quoted_char_in_username_authenticates() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    b.auth_digest_md5("test-realm", "a\"b", SecretString::new("secret".into()));
    let server = b.build().expect("server build");
    let _mount = server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://a%22b:secret@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS");
    let sdp = client
        .describe()
        .expect("DESCRIBE with a quoted-char username must authenticate");
    assert!(!sdp.media.is_empty(), "SDP should advertise media");
    server.stop().ok();
}

#[test]
fn digest_md5_wrong_password_returns_auth_failed() {
    let server = server_with_digest_md5();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:wrong@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS");
    let e = client
        .describe()
        .expect_err("DESCRIBE with wrong pw must fail");
    assert!(matches!(e, RtspError::AuthFailed), "got: {e:?}");
    server.stop().ok();
}

/// Regression guard for the SETUP/PLAY auth gap: the server auth-gates
/// EVERY method per request (like gortsplib/MediaMTX), so a full
/// DESCRIBE → SETUP → PLAY → TEARDOWN session only succeeds if the client
/// attaches credentials to every request — not just DESCRIBE. A client
/// that authenticates only DESCRIBE fails at SETUP with a 401.
#[test]
fn digest_md5_full_session_authenticates_every_method() {
    let server = server_with_digest_md5();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:secret@127.0.0.1:{port}/live?transport=tcp");
    let mut client = RtspClient::connect(&url).expect("connect");
    // OPTIONS first — our server deliberately leaves OPTIONS un-gated
    // (lockout design, session.rs), so this pins that an unchallenged
    // OPTIONS still succeeds through the authenticated send path.
    client.options().expect("OPTIONS before any challenge");
    let sdp = client.describe().expect("DESCRIBE with creds");
    let session = client
        .setup_mp2t_auto(&sdp)
        .expect("SETUP must carry pre-emptive credentials (server challenges every method)");
    let _recv = session.into_recv_transport();
    client.play().expect("PLAY must carry credentials");
    // OPTIONS again — a challenge is now cached, so this request goes out
    // pre-emptively signed; an un-gated server must still 200 it.
    client.options().expect("OPTIONS with cached credentials");
    client.teardown().expect("TEARDOWN must carry credentials");
    server.stop().ok();
}

/// A realm holding a quote and a backslash is escaped on the wire; the
/// client must unescape it for the hash and escape it again when echoing.
#[test]
fn digest_md5_quoted_realm_round_trips() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    b.auth_digest_md5("ts \"q\" \\ r", "admin", SecretString::new("secret".into()));
    let server = b.build().expect("server build");
    let _mount = server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:secret@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    let sdp = client.describe().expect("DESCRIBE against a quoted realm");
    assert!(!sdp.media.is_empty());
    server.stop().ok();
}

/// One DESCRIBE without a credential, then the same DESCRIBE answered the
/// way GStreamer 1.24's `rtspsrc` and `rtspclientsink` answer a Digest MD5
/// challenge: the RFC 2069 form, no `qop`, `nc` or `cnonce`, no `algorithm`.
/// Returns the challenge and the answered response head.
///
/// The credential is the server's own (`auth_digest_md5`, realm
/// `test-realm`) or, with `on_rule`, a `Require` rule's on `/live` under the
/// rules table's challenge (Digest MD5, realm `tstrans`).
///
/// `accept_without_qop` is handed to `accept_digest_without_qop`; `None`
/// never calls it, so the builder's own default decides.
fn gstreamer_style_describe(accept_without_qop: Option<bool>, on_rule: bool) -> (String, String) {
    use crate::fixtures::raw_rtsp::{request, scaled, try_header};
    use md5::{Digest as _, Md5};
    use tst_rtp::{MountCredential, MountPolicy, MountRule};
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    let realm = if on_rule { "tstrans" } else { "test-realm" };
    if !on_rule {
        b.auth_digest_md5(realm, "admin", SecretString::new("secret".into()));
    }
    if let Some(yes) = accept_without_qop {
        b.accept_digest_without_qop(yes);
    }
    let server = b.build().expect("server build");
    server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    if on_rule {
        server
            .mount_rules()
            .set(
                "/live",
                MountRule::both(MountPolicy::Require(vec![MountCredential::new(
                    "admin",
                    SecretString::new("secret".into()),
                )])),
            )
            .expect("rules.set");
    }
    server.start().expect("server start");
    let port = server.local_addr().expect("local_addr").port();
    let uri = format!("rtsp://127.0.0.1:{port}/live");
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(scaled(std::time::Duration::from_secs(5))))
        .unwrap();
    let r = request(
        &mut tcp,
        &format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    );
    let challenge = try_header(&r, "WWW-Authenticate")
        .expect("a challenge")
        .to_owned();
    let nonce = challenge
        .split(',')
        .find_map(|p| {
            p.trim()
                .trim_start_matches("Digest ")
                .strip_prefix("nonce=")
        })
        .expect("a nonce")
        .trim_matches('"')
        .to_owned();
    let md5 = |s: String| -> String {
        Md5::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let ha1 = md5(format!("admin:{realm}:secret"));
    let ha2 = md5(format!("DESCRIBE:{uri}"));
    let response = md5(format!("{ha1}:{nonce}:{ha2}"));
    let answered = request(
        &mut tcp,
        &format!(
            "DESCRIBE {uri} RTSP/1.0\r\nCSeq: 2\r\nAuthorization: Digest username=\"admin\", \
             realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\"\r\n\r\n"
        ),
    );
    server.stop().ok();
    (challenge, answered)
}

#[test]
fn gstreamers_digest_answer_is_accepted_with_the_option() {
    let (challenge, answered) = gstreamer_style_describe(Some(true), false);
    assert!(challenge.contains(r#"qop="auth""#), "{challenge}");
    assert!(answered.starts_with("RTSP/1.0 200"), "{answered}");
}

#[test]
fn gstreamers_digest_answer_is_refused_without_it() {
    use crate::fixtures::raw_rtsp::try_header;
    // The setter is never called: refusing is the default.
    let (challenge, answered) = gstreamer_style_describe(None, false);
    assert!(challenge.contains(r#"qop="auth""#), "{challenge}");
    assert!(answered.starts_with("RTSP/1.0 401"), "{answered}");
    // The same nonce: refused as a downgrade, not as a stale answer.
    let again = try_header(&answered, "WWW-Authenticate").expect("a challenge");
    let nonce = |c: &str| {
        c.split("nonce=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .map(str::to_owned)
    };
    assert_eq!(nonce(again), nonce(&challenge), "{again}");
}

/// The same answer to a `Require` rule's challenge: the option holds for a
/// rule's credential as for the server's own (a publisher ingest set up
/// through the rules table).
#[test]
fn gstreamers_digest_answer_to_a_require_rule_is_accepted_with_the_option() {
    let (challenge, answered) = gstreamer_style_describe(Some(true), true);
    assert!(challenge.contains(r#"realm="tstrans""#), "{challenge}");
    assert!(answered.starts_with("RTSP/1.0 200"), "{answered}");
}

#[test]
fn gstreamers_digest_answer_to_a_require_rule_is_refused_without_it() {
    let (_, answered) = gstreamer_style_describe(Some(false), true);
    assert!(answered.starts_with("RTSP/1.0 401"), "{answered}");
}
