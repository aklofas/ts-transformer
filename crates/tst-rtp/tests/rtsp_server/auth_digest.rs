//! Phase 3 Wave F Task 24 — server-side Digest auth integration tests.
//!
//! Mirror of `rtsp_server_auth_basic.rs` but exercises
//! `RtspServerBuilder::auth_digest_md5` and `auth_digest_sha256`.

use secrecy::SecretString;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_rtp::{RtspClient, RtspError, RtspServer, RtspServerBuilder};

fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().expect("MuxerConfig builds")
}

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
/// percent-decodes it to the literal `a"b` (CORR-21), the client's
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
/// attaches credentials to every request — not just DESCRIBE. The
/// pre-fix client authenticated only DESCRIBE and failed at SETUP with
/// a 401.
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
