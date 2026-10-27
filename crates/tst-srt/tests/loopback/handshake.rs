//! Encrypted + unencrypted handshake; reject-reason matching.
//!
//! Encrypted tests require srt-sys's mbedtls feature, which propagates from
//! tst-srt's `mbedtls` feature (default-on). Without `mbedtls`, libsrt is
//! built with ENABLE_ENCRYPTION=OFF and passphrase-bearing connects fail at
//! the option-application layer rather than the handshake layer.

#![cfg(feature = "mbedtls")]

use std::time::Duration;
use tst_srt::error::RejectReason;
use tst_srt::error::SrtConnectError;
use tst_srt::{Passphrase, SrtListenerBuilder, SrtSocketBuilder};

// Note on negative tests: srt_accept does NOT honor SRTO_RCVTIMEO. A spawned
// listener thread that calls accept() will block forever if no successful
// connection ever arrives — so for tests where we expect the connect to be
// rejected (mismatched passphrase, etc.), we leave the listener idle in the
// main thread and rely on libsrt's internal worker threads to handle the
// handshake-level rejection. The application doesn't need to call accept()
// for the rejection path.

const PASS: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn unencrypted_handshake_succeeds() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let _socket = SrtSocketBuilder::new()
        .recv_timeout(Duration::from_secs(5))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    accept.join();
}

#[test]
fn matching_passphrase_succeeds() {
    require_loopback!();
    let mut builder = SrtListenerBuilder::new();
    builder
        .passphrase(Passphrase::new(PASS).unwrap())
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let _socket = SrtSocketBuilder::new()
        .passphrase(Passphrase::new(PASS).unwrap())
        .recv_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    accept.join();
}

#[test]
fn mismatched_passphrase_rejects_connect() {
    require_loopback!();
    // This test does NOT spawn an accept thread — see file header note: for
    // rejection-path tests we leave the listener idle and rely on libsrt's
    // internal worker threads to handle handshake-level rejection. The
    // Loopback helper assumes an accept thread, so this test keeps a direct
    // SrtListenerBuilder + bind.
    let listener = SrtListenerBuilder::new()
        .passphrase(Passphrase::new("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap())
        .bind("127.0.0.1:0")
        .expect("bind");
    let port = listener.local_addr().unwrap().port();

    let result = SrtSocketBuilder::new()
        .passphrase(Passphrase::new("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB").unwrap())
        .recv_timeout(Duration::from_secs(5))
        .send_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"));

    // Hard assertion: mismatched passphrase must surface as Rejected(BadSecret).
    // The classifier now reads the reject reason from the live socket handle
    // (before srt_close) and gates on SrtErrno::Setup | SrtErrno::Connection,
    // so SRT_ECONNREJ (major 1 → Setup) + SRT_REJ_BADSECRET (reason 10) is
    // correctly mapped to Rejected { reason: BadSecret }.
    match result {
        Err(SrtConnectError::Rejected {
            reason: RejectReason::BadSecret,
            ..
        }) => { /* expected */ }
        other => panic!("expected Rejected(BadSecret), got {other:?}"),
    }

    drop(listener);
}
