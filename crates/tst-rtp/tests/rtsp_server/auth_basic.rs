//! Server-side Basic auth integration tests.
//!
//! Exercises the `RtspServerBuilder::auth_basic` surface end-to-end
//! against the `RtspClient`. Each test spins up a fresh tokio-
//! backed `RtspServer` on a kernel-picked port, runs an RTSP request,
//! and asserts on the response or returned error.

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

/// Spin up an `RtspServer` requiring Basic auth, with a single
/// `/live` unicast mount, listening on a kernel-picked port.
fn server_with_basic_auth() -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    b.auth_basic("test-realm", "admin", SecretString::new("secret".into()));
    let server = b.build().expect("server build");
    let _mount = server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    server
}

/// No credentials in the URL → DESCRIBE returns AuthFailed (the client
/// observes a 401 with WWW-Authenticate but has no creds to retry with).
#[test]
fn no_credentials_returns_401() {
    let server = server_with_basic_auth();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    // OPTIONS isn't auth-gated per the server handler; should succeed.
    client.options().expect("OPTIONS without auth");
    // DESCRIBE without credentials → the client can't form an
    // Authorization retry, so `handle_auth_challenge_and_retry` returns
    // RtspError::AuthFailed at the missing-username step.
    let e = client
        .describe()
        .expect_err("DESCRIBE without creds must 401");
    assert!(matches!(e, RtspError::AuthFailed), "got: {e:?}");
    server.stop().ok();
}

/// Valid URL-embedded credentials → DESCRIBE succeeds.
#[test]
fn valid_credentials_succeed() {
    let server = server_with_basic_auth();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:secret@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS");
    let sdp = client.describe().expect("DESCRIBE with creds");
    assert!(!sdp.media.is_empty(), "SDP should advertise media");
    server.stop().ok();
}

/// Wrong password → AuthFailed. The client retries once with the bad
/// credential, the server returns a second 401, and the client surfaces
/// `RtspError::AuthFailed`.
#[test]
fn wrong_password_returns_auth_failed() {
    let server = server_with_basic_auth();
    let port = server.local_addr().expect("local_addr after start").port();
    let url = format!("rtsp://admin:wrong-pw@127.0.0.1:{port}/live");
    let mut client = RtspClient::connect(&url).expect("connect");
    client.options().expect("OPTIONS");
    let e = client
        .describe()
        .expect_err("DESCRIBE with wrong pw must fail");
    assert!(matches!(e, RtspError::AuthFailed), "got: {e:?}");
    server.stop().ok();
}
