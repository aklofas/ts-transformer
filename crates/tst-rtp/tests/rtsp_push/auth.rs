//! A push to a server that wants credentials: the builder's credential
//! (Basic, Digest MD5, Digest SHA-256) and a mount rule's. The server gates
//! ANNOUNCE, every SETUP, RECORD and the TEARDOWN, so a push that sends and
//! closes cleanly answered every challenge.

use tst_rtp::{
    AuthScheme, AuthSchemes, MountCredential, MountPolicy, MountRule, PushDescription,
    RtspClientBuilder, RtspError, RtspPush, SecretString,
};

use crate::common::*;
use crate::fixtures::tcp_relay::TcpRelay;

const USER: &str = "publisher";
const PASSWORD: &str = "s3cret-for-the-push";

fn password(p: &str) -> SecretString {
    SecretString::new(p.into())
}

/// Push a few packets with `builder`'s credential, see them on the mount,
/// and close: every step answered its challenge.
fn push_and_close(s: &Served, builder: RtspClientBuilder) {
    let mut app = s.app();
    let mut push = RtspPush::connect(builder, &PushDescription::Mp2t).unwrap();
    let ts = ts_packets(7, 0x22);
    push.send_ts(&ts).unwrap();
    assert_same_bytes(&read_ts(&mut app, ts.len()), &ts, "the mount's stream");
    push.close().unwrap();
    s.until_no_publisher();
}

/// A server whose builder credential, of `scheme`, every request must
/// answer.
fn served_with_a_credential(scheme: AuthScheme) -> Served {
    Served::with(
        |b| {
            let realm = "push";
            match scheme {
                AuthScheme::Basic => b.auth_basic(realm, USER, password(PASSWORD)),
                AuthScheme::DigestMd5 => b.auth_digest_md5(realm, USER, password(PASSWORD)),
                AuthScheme::DigestSha256 => b.auth_digest_sha256(realm, USER, password(PASSWORD)),
                other => panic!("no test server for {other:?}"),
            };
        },
        Default::default(),
    )
}

#[test]
fn basic_through_the_builder_credential() {
    let s = served_with_a_credential(AuthScheme::Basic);
    push_and_close(&s, s.builder().auth(USER, password(PASSWORD)));
}

#[test]
fn digest_md5_through_the_builder_credential() {
    let s = served_with_a_credential(AuthScheme::DigestMd5);
    push_and_close(&s, s.builder().auth(USER, password(PASSWORD)));
}

#[test]
fn digest_sha256_through_the_builder_credential() {
    let s = served_with_a_credential(AuthScheme::DigestSha256);
    let digest_only = s
        .builder()
        .auth(USER, password(PASSWORD))
        .auth_schemes(AuthSchemes::DigestOnly);
    push_and_close(&s, digest_only);
}

/// A server with no credential of its own, whose rule for the mount wants
/// one for publishing.
fn served_with_a_publish_rule() -> Served {
    let s = Served::new();
    let wanted = MountPolicy::Require(vec![MountCredential::new(USER, password(PASSWORD))]);
    s.server
        .mount_rules()
        .set(PATH, MountRule::new(wanted, MountPolicy::Open))
        .unwrap();
    s
}

#[test]
fn a_mount_rule_credential_admits_the_push() {
    let s = served_with_a_publish_rule();
    push_and_close(&s, s.builder().auth(USER, password(PASSWORD)));
}

/// A client limited to Digest meets a rule that challenges with Basic: it
/// gives up, and the password never crosses the wire, which a relay
/// between the two shows.
#[test]
fn digest_only_sends_no_password_to_a_basic_only_rule() {
    let s = served_with_a_publish_rule();
    s.server
        .mount_rules()
        .set_challenge(AuthScheme::Basic, "push")
        .unwrap();
    let relay = TcpRelay::to(s.server.local_addr().unwrap());
    let url = format!("rtsp://127.0.0.1:{}{PATH}", relay.port());
    let builder = RtspClientBuilder::new(&url)
        .unwrap()
        .auth(USER, password(PASSWORD))
        .auth_schemes(AuthSchemes::DigestOnly);
    let r = RtspPush::connect(builder, &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::AuthUnsupported { .. })),
        "{:?}",
        r.map(|_| "a push")
    );
    // All the client wrote came before the challenge it gave up on, so the
    // relay has passed it on.
    let challenge = String::from_utf8_lossy(&relay.downstream()).to_ascii_lowercase();
    assert!(challenge.contains("www-authenticate: basic"), "{challenge}");
    let sent = String::from_utf8_lossy(&relay.upstream()).to_ascii_lowercase();
    assert!(sent.starts_with("announce "), "{sent}");
    assert!(!sent.contains("authorization"), "{sent}");
    assert!(!sent.contains(&PASSWORD.to_ascii_lowercase()), "{sent}");
    assert!(s.mount.publisher().is_none());
}

#[test]
fn a_wrong_password_is_auth_failed() {
    let s = served_with_a_credential(AuthScheme::DigestMd5);
    let wrong = s.builder().auth(USER, password("not-the-password"));
    let r = RtspPush::connect(wrong, &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::AuthFailed)),
        "{:?}",
        r.map(|_| "a push")
    );
    assert!(s.mount.publisher().is_none());
}

/// Every server the tests above push to refuses a push that brings no
/// credential, so their pushes pass because they answered the challenge.
/// A client without credentials reports the challenge as `AuthFailed`.
#[test]
fn a_push_without_credentials_is_refused() {
    let servers = [
        ("Basic", served_with_a_credential(AuthScheme::Basic)),
        (
            "Digest MD5",
            served_with_a_credential(AuthScheme::DigestMd5),
        ),
        (
            "Digest SHA-256",
            served_with_a_credential(AuthScheme::DigestSha256),
        ),
        ("the mount rule", served_with_a_publish_rule()),
    ];
    for (which, s) in &servers {
        let r = RtspPush::connect(s.builder(), &PushDescription::Mp2t);
        assert!(
            matches!(&r, Err(RtspError::AuthFailed)),
            "{which}: {:?}",
            r.map(|_| "a push")
        );
        assert!(s.mount.publisher().is_none(), "{which}");
    }
}
