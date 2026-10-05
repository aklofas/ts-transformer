//! Regression: the keepalive cadence must follow the server-advertised
//! session timeout, and post-SETUP pings must be bound to the session.
//!
//! Before the retune, the keepalive thread was spawned at connect
//! time with its interval frozen from the DEFAULT 60 s session timeout —
//! a `Session: <id>;timeout=N` parsed later at SETUP updated the client
//! field but never reached the running thread, so any server advertising
//! `timeout < 60` expired the session between 30 s pings. Separately, the
//! session-id cell shared with the thread was never written after SETUP,
//! so keepalive OPTIONS never carried a `Session:` header at all — and an
//! un-bound OPTIONS does not refresh the server's session timer
//! (RFC 7826 §10.5 defines keep-alive in terms of a request carrying the
//! session identifier).

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::fixtures::rtsp_loopback_server::*;

/// Server advertises `timeout=1` → the auto-keepalive (spawned at connect
/// against the 60 s default → 30 s cadence) must retune to 500 ms at
/// SETUP and bind its pings to the session. In the 2.5 s observation
/// window the retuned cadence yields ~4 session-bound pings; a frozen
/// 30 s cadence yields zero.
#[test]
fn keepalive_retunes_to_server_advertised_timeout() {
    let cfg = FixtureConfig {
        setup_timeout_secs: Some(1),
        ..FixtureConfig::default()
    };
    let h = FixtureHandle::spawn(cfg);
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);

    // Production path: the builder spawns the auto-keepalive at connect
    // with NO interval override, so the cadence must derive from the
    // negotiated session timeout.
    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();

    std::thread::sleep(Duration::from_millis(2500));
    let with_session = h.options_with_session.load(Ordering::Relaxed);
    drop(client);
    drop(h);
    assert!(
        with_session >= 2,
        "expected ≥2 session-bound keepalive pings in 2.5 s at the retuned \
         500 ms cadence (server advertised timeout=1), saw {with_session}"
    );
}

/// Keepalive pings authenticate end-to-end against a server that
/// challenges OPTIONS per-request: the fixture 401s any OPTIONS
/// without a valid `Authorization`, so the session-bound ping count only
/// climbs if each ping pre-emptively signs with the challenge cached at
/// DESCRIBE. Uses SHA-256 + `qop="auth"` so the per-ping nonce-count
/// allocation is exercised too.
#[test]
fn keepalive_pings_authenticate_against_challenged_options() {
    let cfg = FixtureConfig {
        auth: AuthMode::DigestSha256,
        challenge_options: true,
        setup_timeout_secs: Some(1),
        ..FixtureConfig::default()
    };
    let h = FixtureHandle::spawn(cfg);
    // Credentials via URL userinfo — the fixture's defaults.
    let url = format!("rtsp://admin:secret@127.0.0.1:{}/test", h.port);

    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();

    std::thread::sleep(Duration::from_millis(2500));
    let with_session = h.options_with_session.load(Ordering::Relaxed);
    drop(client);
    drop(h);
    assert!(
        with_session >= 2,
        "expected ≥2 authenticated session-bound keepalive pings in 2.5 s \
         (server 401s any unauthorized OPTIONS), saw {with_session}"
    );
}

/// A builder-supplied `keepalive_interval` override outranks the
/// server-advertised timeout: with a 10 s override and a 1 s server
/// timeout, no ping fires inside a 2 s window (the first override-paced
/// ping is at ~10 s). Guards the override contract against a retune that
/// clobbers it.
#[test]
fn builder_interval_override_wins_over_server_timeout() {
    let cfg = FixtureConfig {
        setup_timeout_secs: Some(1),
        ..FixtureConfig::default()
    };
    let h = FixtureHandle::spawn(cfg);
    let url = format!("rtsp://127.0.0.1:{}/test", h.port);

    let mut client = tst_rtp::RtspClientBuilder::new(&url)
        .unwrap()
        .keepalive_interval(Duration::from_secs(10))
        .connect()
        .unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();

    std::thread::sleep(Duration::from_millis(2000));
    let total = h.options_total.load(Ordering::Relaxed);
    drop(client);
    drop(h);
    assert_eq!(
        total, 0,
        "no keepalive ping should fire within 2 s of a 10 s override \
         (a retune must not clobber the builder-supplied interval)"
    );
}
