//! The per-path rules table: handle semantics against a live server, the
//! builder's default rule, and the policy every gated request is checked
//! against (publish side over raw ANNOUNCEs, read side through `RtspClient`).

use std::cell::Cell;
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine as _;
use md5::{Digest as _, Md5};
use secrecy::SecretString;
use tst_rtp::{
    AuthScheme, MountCredential, MountPolicy, MountRule, MountRuleError, RefuseStatus, RtspClient,
    RtspError, RtspServer, RtspServerBuilder,
};

use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, scaled, try_header};
use crate::fixtures::raw_rtsp_publisher::*;

fn started(b: RtspServerBuilder) -> RtspServer {
    let server = b.build().unwrap();
    server.start().unwrap();
    server
}

/// A started server with a muxer-backed (read) mount at `/a`.
fn started_with_mount(b: RtspServerBuilder) -> RtspServer {
    let server = b.build().unwrap();
    server.add_mount("/a", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    server
}

/// Two handles are one table, and `stop()` closes it.
#[test]
fn mount_rules_handle_is_live_across_clones_and_closes_with_stop() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let a = server.mount_rules();
    let b = server.mount_rules();
    a.set("/amber", MountRule::both(MountPolicy::Open)).unwrap();
    assert_eq!(b.len(), 1);
    assert!(!b.is_closed());

    server.stop().unwrap();

    assert!(matches!(
        a.set("/other", MountRule::default()),
        Err(MountRuleError::Closed)
    ));
    assert!(b.is_closed());
    assert_eq!(b.len(), 1);
}

/// The builder's default rule governs a path with no entry.
#[test]
fn builder_default_mount_rule_seeds_the_table() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.accept_unregistered_publishers(true);
    b.default_mount_rule(MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)));
    let server = started(b);
    let port = server.local_addr().unwrap().port();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/anything", SDP_MP2T), 404);
    assert!(
        server
            .next_publisher(Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "a refused ANNOUNCE queues no mount"
    );
    assert_eq!(server.stats().mounts, 0);

    // A rule for the path lifts the default; Inherit with no builder
    // credential is open, so the on-demand mount is created as before.
    server
        .mount_rules()
        .set("/new", MountRule::default())
        .unwrap();
    assert_eq!(p.announce("/new", SDP_MP2T), 200);
    let handle = server
        .next_publisher(scaled(Duration::from_secs(2)))
        .unwrap()
        .expect("the admitted ANNOUNCE queues its mount");
    assert_eq!(handle.mount_path(), "/new");
    server.stop().unwrap();
}

fn pw(s: &str) -> SecretString {
    SecretString::new(s.into())
}

fn cred(user: &str, password: &str) -> MountCredential {
    MountCredential::new(user, pw(password))
}

fn status(resp: &str) -> u16 {
    resp.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn connect(port: u16) -> TcpStream {
    let t = TcpStream::connect(("127.0.0.1", port)).unwrap();
    t.set_read_timeout(Some(scaled(Duration::from_secs(2))))
        .unwrap();
    t
}

fn announce(tcp: &mut TcpStream, uri: &str, cseq: u32, auth: &str) -> String {
    request(
        tcp,
        &format!(
            "ANNOUNCE {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{SDP_MP2T}",
            SDP_MP2T.len()
        ),
    )
}

fn basic(user: &str, password: &str) -> String {
    format!(
        "Authorization: Basic {}\r\n",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

/// One parameter of a `WWW-Authenticate: Digest ...` challenge.
fn challenge_param(ch: &str, k: &str) -> Option<String> {
    ch.split(',').find_map(|p| {
        let (pk, pv) = p
            .trim()
            .trim_start_matches("Digest ")
            .trim()
            .split_once('=')?;
        (pk.trim() == k).then(|| pv.trim().trim_matches('"').to_owned())
    })
}

/// A Digest-MD5 `Authorization` line answering `challenge`. `nc` is the
/// connection's nonce-count, advanced on every call (the server's
/// high-water mark rises even on a refused digest).
fn digest_md5(
    challenge: &str,
    nc: &Cell<u32>,
    user: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> String {
    let md5 = |s: &str| -> String {
        Md5::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let realm = challenge_param(challenge, "realm").unwrap();
    let nonce = challenge_param(challenge, "nonce").unwrap();
    nc.set(nc.get() + 1);
    let nc = format!("{:08x}", nc.get());
    let ha1 = md5(&format!("{user}:{realm}:{password}"));
    let ha2 = md5(&format!("{method}:{uri}"));
    let resp = md5(&format!("{ha1}:{nonce}:{nc}:cafebabe:auth:{ha2}"));
    format!(
        r#"Authorization: Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{resp}", algorithm=MD5, qop=auth, nc={nc}, cnonce="cafebabe""#
    ) + "\r\n"
}

/// A `Require` publish rule on a server with no builder credential
/// challenges with the table's default (Digest MD5, realm `tstrans`), and
/// `set_challenge` switches the scheme the rule admits.
#[test]
fn require_on_publish_challenges_then_admits_basic() {
    let server = started_with_mount(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let mount = server.add_publish_mount("/amber").unwrap();
    server
        .mount_rules()
        .set(
            "/amber",
            MountRule::new(
                MountPolicy::Require(vec![cred("amber", "pw")]),
                MountPolicy::Inherit,
            ),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/amber");

    let mut tcp = connect(port);
    let r = announce(&mut tcp, &uri, 1, "");
    assert_eq!(status(&r), 401, "{r}");
    assert!(
        try_header(&r, "WWW-Authenticate")
            .is_some_and(|h| h.starts_with("Digest realm=\"tstrans\"")),
        "{r}"
    );
    assert!(mount.publisher().is_none());
    let r = announce(&mut tcp, &uri, 2, &basic("amber", "pw"));
    assert_eq!(status(&r), 401, "the challenge scheme is Digest: {r}");
    drop(tcp);

    server
        .mount_rules()
        .set_challenge(AuthScheme::Basic, "lab")
        .unwrap();
    let mut tcp = connect(port);
    let r = announce(&mut tcp, &uri, 1, "");
    assert_eq!(status(&r), 401, "{r}");
    assert_eq!(
        try_header(&r, "WWW-Authenticate"),
        Some("Basic realm=\"lab\""),
        "{r}"
    );
    let r = announce(&mut tcp, &uri, 2, &basic("amber", "pw"));
    assert_eq!(status(&r), 200, "{r}");
    assert!(mount.publisher().is_some());
    drop(tcp);
    server.stop().unwrap();
}

/// `Require` replaces the credential set for its path and direction: the
/// builder's own user no longer opens it. The challenge keeps the builder's
/// scheme and realm.
#[test]
fn require_on_publish_admits_digest_md5_and_refuses_the_builder_credential() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_md5("realm", "admin", pw("secret"));
    let server = started_with_mount(b);
    let mount = server.add_publish_mount("/amber").unwrap();
    server
        .mount_rules()
        .set(
            "/amber",
            MountRule::new(
                MountPolicy::Require(vec![cred("amber", "pw")]),
                MountPolicy::Inherit,
            ),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/amber");
    let nc = Cell::new(0);

    let mut tcp = connect(port);
    let r = announce(&mut tcp, &uri, 1, "");
    assert_eq!(status(&r), 401, "{r}");
    let ch = try_header(&r, "WWW-Authenticate")
        .expect("challenge")
        .to_owned();
    assert!(ch.starts_with("Digest"), "{ch}");
    assert_eq!(
        challenge_param(&ch, "realm").as_deref(),
        Some("realm"),
        "the challenge comes from the builder's auth"
    );
    let r = announce(
        &mut tcp,
        &uri,
        2,
        &digest_md5(&ch, &nc, "admin", "secret", "ANNOUNCE", &uri),
    );
    assert_eq!(
        status(&r),
        401,
        "the builder's user does not open a Require path: {r}"
    );
    let r = announce(
        &mut tcp,
        &uri,
        3,
        &digest_md5(&ch, &nc, "amber", "pw", "ANNOUNCE", &uri),
    );
    assert_eq!(status(&r), 200, "{r}");
    assert!(mount.publisher().is_some());
    drop(tcp);
    server.stop().unwrap();
}

/// `Inherit` keeps the builder's credential; `Open` waives it for one
/// direction of one path.
#[test]
fn inherit_keeps_the_builder_credential_and_open_waives_it() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_basic("realm", "admin", pw("secret"));
    let server = started_with_mount(b);
    let port = server.local_addr().unwrap().port();

    let mut anon = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/a")).unwrap();
    let e = anon
        .describe()
        .expect_err("Inherit keeps the builder's credential");
    assert!(matches!(e, RtspError::AuthFailed), "got: {e:?}");
    let mut admin =
        RtspClient::connect(&format!("rtsp://admin:secret@127.0.0.1:{port}/a")).unwrap();
    admin
        .describe()
        .expect("DESCRIBE with the builder's credential");

    server
        .mount_rules()
        .set(
            "/a",
            MountRule::new(MountPolicy::Inherit, MountPolicy::Open),
        )
        .unwrap();
    let mut anon = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/a")).unwrap();
    anon.describe()
        .expect("Open read waives the builder's credential");
    server.stop().unwrap();
}

/// With no builder credential, `Inherit` is open: the read side of a path
/// whose publishers must authenticate stays open.
#[test]
fn read_inherit_is_open_when_the_builder_has_no_credential_while_publish_requires() {
    let server = started_with_mount(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let _mount = server.add_publish_mount("/amber").unwrap();
    server
        .mount_rules()
        .set(
            "/amber",
            MountRule::new(
                MountPolicy::Require(vec![cred("amber", "pw")]),
                MountPolicy::Inherit,
            ),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();

    let mut reader = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/amber")).unwrap();
    reader.describe().expect("read side stays open");

    let mut tcp = connect(port);
    let r = announce(&mut tcp, &format!("rtsp://127.0.0.1:{port}/amber"), 1, "");
    assert_eq!(status(&r), 401, "{r}");
    assert!(try_header(&r, "WWW-Authenticate").is_some(), "{r}");
    drop(tcp);
    server.stop().unwrap();
}

/// A `Require` read rule gates DESCRIBE, SETUP and PLAY, challenged with the
/// table's scheme (here Digest SHA-256) on a credential-less server.
#[test]
fn read_require_with_digest_sha256_via_rtsp_client() {
    let server = started_with_mount(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let rules = server.mount_rules();
    rules
        .set(
            "/a",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Require(vec![cred("viewer", "pw")]),
            ),
        )
        .unwrap();
    rules
        .set_challenge(AuthScheme::DigestSha256, "lab")
        .unwrap();
    let port = server.local_addr().unwrap().port();

    let mut viewer = RtspClient::connect(&format!(
        "rtsp://viewer:pw@127.0.0.1:{port}/a?transport=tcp"
    ))
    .unwrap();
    let sdp = viewer
        .describe()
        .expect("DESCRIBE with the rule's credential");
    let session = viewer.setup_mp2t_auto(&sdp).expect("SETUP");
    let _recv = session.into_recv_transport();
    viewer.play().expect("PLAY");

    let mut anon = RtspClient::connect(&format!("rtsp://127.0.0.1:{port}/a")).unwrap();
    let e = anon
        .describe()
        .expect_err("Require read refuses no credential");
    assert!(matches!(e, RtspError::AuthFailed), "got: {e:?}");
    server.stop().unwrap();
}

/// A `Refuse` publish rule answers at once with its status and no
/// challenge, before the on-demand path could create or queue a mount.
#[test]
fn refuse_answers_before_any_mount_is_created_or_claimed() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.accept_unregistered_publishers(true);
    let server = started(b);
    server
        .mount_rules()
        .set(
            "/taken",
            MountRule::new(
                MountPolicy::Refuse(RefuseStatus::NotEnoughBandwidth),
                MountPolicy::Inherit,
            ),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let mounts_before = server.stats().mounts;

    let mut tcp = connect(port);
    let r = announce(&mut tcp, &format!("rtsp://127.0.0.1:{port}/taken"), 1, "");
    assert_eq!(status(&r), 453, "{r}");
    assert!(try_header(&r, "WWW-Authenticate").is_none(), "{r}");
    assert_eq!(server.stats().mounts, mounts_before);
    assert!(
        server
            .next_publisher(Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "a refused ANNOUNCE queues no mount"
    );

    let r = announce(&mut tcp, &format!("rtsp://127.0.0.1:{port}/free"), 2, "");
    assert_eq!(status(&r), 200, "{r}");
    let handle = server
        .next_publisher(scaled(Duration::from_secs(2)))
        .unwrap()
        .expect("the admitted ANNOUNCE queues its mount");
    assert_eq!(handle.mount_path(), "/free");
    drop(tcp);
    server.stop().unwrap();
}

/// A Refuse status is not a 401, so it never counts toward the
/// three-consecutive-401 close.
#[test]
fn a_refuse_status_never_trips_the_three_401_close() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let _mount = server.add_publish_mount("/x").unwrap();
    server
        .mount_rules()
        .set(
            "/x",
            MountRule::both(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();

    let mut p = RawPublisher::connect(port);
    for _ in 0..4 {
        assert_eq!(p.announce("/x", SDP_MP2T), 403);
    }
    assert_eq!(p.options("/x"), 200, "the connection stays open");
    server.stop().unwrap();
}
