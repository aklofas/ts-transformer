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

use crate::fixtures::raw_rtsp::{
    in_session, make_muxer_cfg, request, scaled, session_id, try_header,
};
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

/// A mount and a rule can only exist under one key, so a path the server
/// resolves differently from its registration cannot be served past the
/// rule. `add_mount("/live/")` is refused; `/live` with a `Refuse` read rule
/// answers 403 for `/live`, `/live/` and `/live//` alike.
#[test]
fn a_mount_and_its_rule_share_one_key() {
    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .unwrap()
        .build()
        .unwrap();
    assert!(matches!(
        server.add_mount("/live/", make_muxer_cfg()),
        Err(tst_rtp::RtspServerError::InvalidMountPath { .. })
    ));
    server.add_mount("/live", make_muxer_cfg()).unwrap();
    server
        .mount_rules()
        .set(
            "/live",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::Forbidden),
            ),
        )
        .unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    for suffix in ["/live", "/live/", "/live//"] {
        let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
            .unwrap();
        let resp = request(
            &mut tcp,
            &format!("DESCRIBE rtsp://127.0.0.1:{port}{suffix} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
        );
        assert!(resp.starts_with("RTSP/1.0 403"), "{suffix}: {resp}");
    }
    server.stop().unwrap();
}

/// A path rule's `Inherit` side takes the table's default rule: a default of
/// `Refuse(Forbidden)` refuses a DESCRIBE of `/a` whose rule opens only the
/// publish side.
#[test]
fn a_path_rules_inherit_read_side_takes_the_default_refuse() {
    let server = started_with_mount(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let rules = server.mount_rules();
    rules
        .set_default(MountRule::both(MountPolicy::Refuse(
            RefuseStatus::Forbidden,
        )))
        .unwrap();
    rules
        .set(
            "/a",
            MountRule::new(MountPolicy::Open, MountPolicy::Inherit),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let resp = request(
        &mut tcp,
        &format!("DESCRIBE rtsp://127.0.0.1:{port}/a RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    );
    assert!(resp.starts_with("RTSP/1.0 403"), "{resp}");
    server.stop().unwrap();
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

/// A handle that outlives its server is closed when the server is dropped.
#[test]
fn mount_rules_handle_closes_when_the_server_is_dropped() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let rules = server.mount_rules();
    drop(server);
    assert!(rules.is_closed());
    assert!(matches!(
        rules.set("/amber", MountRule::default()),
        Err(MountRuleError::Closed)
    ));
}

/// A hard cancel through the cancel handle closes the table once the
/// listener has exited.
#[test]
fn mount_rules_handle_closes_on_a_hard_cancel() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let rules = server.mount_rules();
    server.cancel_handle().cancel();
    assert!(eventually(|| rules.is_closed()), "the table closes");
    assert!(matches!(
        rules.set("/amber", MountRule::default()),
        Err(MountRuleError::Closed)
    ));
    drop(server);
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

    // A path rule's Inherit takes the default rule, so it does not lift
    // the refusal.
    server
        .mount_rules()
        .set("/new", MountRule::default())
        .unwrap();
    assert_eq!(p.announce("/new", SDP_MP2T), 404);
    // An Open rule for the path lifts the default, so the on-demand mount is
    // created as before.
    server
        .mount_rules()
        .set("/new", MountRule::both(MountPolicy::Open))
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
    announce_sdp(tcp, uri, cseq, auth, SDP_MP2T)
}

fn announce_sdp(tcp: &mut TcpStream, uri: &str, cseq: u32, auth: &str, sdp: &str) -> String {
    request(
        tcp,
        &format!(
            "ANNOUNCE {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
            sdp.len()
        ),
    )
}

/// SETUP `mode=record` over TCP-interleaved, by hand so it can carry `auth`.
fn setup_record(tcp: &mut TcpStream, uri: &str, cseq: u32, auth: &str) -> String {
    request(
        tcp,
        &format!(
            "SETUP {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n"
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

/// Poll `cond` until it holds or a scaled deadline passes.
fn eventually(cond: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + scaled(Duration::from_secs(5));
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// A rule edit reaches a playing reader at its next role-keyed request: a
/// refused PAUSE answers the Refuse status and ends the reader's role, so
/// its subscription to the mount goes away.
#[test]
fn a_refused_pause_ends_a_playing_readers_role() {
    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .unwrap()
        .build()
        .unwrap();
    let mount = server.add_mount("/a", make_muxer_cfg()).unwrap();
    server.start().unwrap();
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
    let port = server.local_addr().unwrap().port();

    let mut viewer = RtspClient::connect(&format!(
        "rtsp://viewer:pw@127.0.0.1:{port}/a?transport=tcp"
    ))
    .unwrap();
    let sdp = viewer.describe().expect("DESCRIBE");
    let session = viewer.setup_mp2t_auto(&sdp).expect("SETUP");
    let _recv = session.into_recv_transport();
    viewer.play().expect("PLAY");
    assert!(eventually(|| mount.peer_count() == 1), "the reader plays");

    rules
        .set(
            "/a",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::ServiceUnavailable),
            ),
        )
        .unwrap();
    let e = viewer
        .pause()
        .expect_err("the refused read side answers 503");
    assert!(
        matches!(e, RtspError::Protocol { code: 503, .. }),
        "got: {e:?}"
    );
    assert!(
        eventually(|| mount.peer_count() == 0),
        "the refused reader's role ended"
    );
    server.stop().unwrap();
}

fn publish_rule(policy: MountPolicy) -> MountRule {
    MountRule::new(policy, MountPolicy::Inherit)
}

/// A publisher's track SETUP (`/amber/streamid=0`) keys on the mount its
/// ANNOUNCE claimed and publishes: it is challenged by the `/amber`
/// publish rule even though the read side is open. An idle connection's
/// SETUP on the same URI keys on the URI and reads, so it is never
/// challenged.
#[test]
fn a_publishers_track_setup_is_keyed_on_the_announce_path() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let mount = server.add_publish_mount("/amber").unwrap();
    let rules = server.mount_rules();
    rules
        .set(
            "/amber",
            publish_rule(MountPolicy::Require(vec![cred("amber", "pw")])),
        )
        .unwrap();
    rules.set_challenge(AuthScheme::Basic, "lab").unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/amber");
    let track = format!("{uri}/streamid=0");
    let auth = basic("amber", "pw");

    let mut tcp = connect(port);
    let r = announce_sdp(&mut tcp, &uri, 1, &auth, SDP_H264);
    assert_eq!(status(&r), 200, "{r}");
    assert!(mount.publisher().is_some());
    let r = setup_record(&mut tcp, &track, 2, "");
    assert_eq!(
        status(&r),
        401,
        "the publish rule gates the track SETUP: {r}"
    );
    let r = setup_record(&mut tcp, &track, 3, &auth);
    assert_eq!(status(&r), 200, "{r}");

    let mut idle = connect(port);
    let r = setup_record(&mut idle, &track, 1, "");
    assert!(
        matches!(status(&r), 404 | 455),
        "an idle SETUP keys on the open read side: {r}"
    );
    drop((tcp, idle));
    server.stop().unwrap();
}

/// A publisher that SETUPs a track under a mount other than the one it
/// announced gets the answer a missing mount gets, so a `Refuse(NotFound)`
/// rule on the other mount cannot be probed through it.
#[test]
fn a_publishers_setup_outside_its_announced_mount_answers_like_a_missing_mount() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    server.add_publish_mount("/amber").unwrap();
    server.add_publish_mount("/hidden").unwrap();
    server
        .mount_rules()
        .set(
            "/hidden",
            MountRule::new(
                MountPolicy::Refuse(RefuseStatus::NotFound),
                MountPolicy::Refuse(RefuseStatus::NotFound),
            ),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/amber");

    let mut tcp = connect(port);
    let r = announce_sdp(&mut tcp, &uri, 1, "", SDP_H264);
    assert_eq!(status(&r), 200, "{r}");
    let hidden = setup_record(
        &mut tcp,
        &format!("rtsp://127.0.0.1:{port}/hidden/streamid=0"),
        2,
        "",
    );
    let missing = setup_record(
        &mut tcp,
        &format!("rtsp://127.0.0.1:{port}/does-not-exist/streamid=0"),
        3,
        "",
    );
    assert_eq!(status(&hidden), 404, "{hidden}");
    assert_eq!(status(&missing), 404, "{missing}");
    // Same status line, same headers (CSeq aside), same body.
    let strip = |r: &str| {
        r.lines()
            .filter(|l| !l.to_ascii_lowercase().starts_with("cseq"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&hidden), strip(&missing));
    drop(tcp);
    server.stop().unwrap();
}

/// A rule turned to `Refuse` reaches a publisher that already holds the
/// slot: its next gated request answers the Refuse status and frees the
/// slot; a fresh ANNOUNCE is refused too.
#[test]
fn a_rule_turned_to_refuse_ends_the_publishers_role_at_its_next_gated_request() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let mount = server.add_publish_mount("/amber").unwrap();
    let port = server.local_addr().unwrap().port();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/amber", SDP_H264), 200);
    assert!(mount.publisher().is_some(), "the ANNOUNCE holds the slot");

    server
        .mount_rules()
        .set(
            "/amber",
            publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
    assert_eq!(p.setup_interleaved("/amber", "streamid=0").0, 403);
    assert!(
        mount.publisher().is_none(),
        "the refused SETUP freed the slot before its response"
    );

    let mut q = RawPublisher::connect(port);
    assert_eq!(q.announce("/amber", SDP_H264), 403);
    server.stop().unwrap();
}

const PT: u8 = 96;
const SSRC: u32 = 0x0F0F_1234;
const TS0: u32 = 1_000_000;

/// OPTIONS and GET_PARAMETER are ungated, so a streaming publisher's
/// keepalives never see a rule change and its media keeps flowing; its
/// TEARDOWN is gated, answers the Refuse status and frees the slot.
#[test]
fn a_streaming_publishers_keepalive_ignores_a_rule_change_but_teardown_sees_it() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let mount = server.add_publish_mount("/amber").unwrap();
    let port = server.local_addr().unwrap().port();
    let mut app_t = mount.clone().into_recv_transport().unwrap();
    // Quiet detector, unscaled like the elementary publish tests.
    app_t.set_recv_timeout(Some(Duration::from_secs(2)));
    let mut app = tst_pipeline::DemuxReceiver::new(app_t);

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/amber", SDP_H264), 200);
    let (st, pair) = p.setup_interleaved_pair("/amber", "streamid=0");
    assert_eq!(st, 200);
    let (rtp_ch, rtcp_ch) = pair.expect("server allocated an interleaved pair");
    assert_eq!(p.record_from_start("/amber"), 200);

    // 20 AUs: a 3-fragment IDR plus 19 single-packet P slices. The first
    // 12 packets are the first 10 AUs.
    let packets = h264_rtp_packets(20, PT, 100, SSRC, TS0);
    assert_eq!(packets.len(), 22);
    p.send_frame(rtcp_ch, &sr_packet(SSRC, 3_900_000_000, 0, TS0));
    for pkt in &packets[..12] {
        p.send_frame(rtp_ch, pkt);
    }
    // The depacketizer emits an AU when the next one starts: 9 of 10.
    assert!(
        eventually(|| mount.stats().aus_emitted >= 9),
        "first batch: {} AUs",
        mount.stats().aus_emitted
    );

    server
        .mount_rules()
        .set(
            "/amber",
            publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
    assert_eq!(p.get_parameter("/amber"), 200);
    assert_eq!(p.options("/amber"), 200);
    for pkt in &packets[12..] {
        p.send_frame(rtp_ch, pkt);
    }
    assert!(
        eventually(|| mount.stats().aus_emitted >= 19),
        "second batch still delivered: {} AUs",
        mount.stats().aus_emitted
    );
    assert!(mount.publisher().is_some(), "keepalives ended nothing");

    assert_eq!(p.teardown("/amber"), 403);
    assert!(
        mount.publisher().is_none(),
        "the refused TEARDOWN freed the slot"
    );

    let deadline = std::time::Instant::now() + scaled(Duration::from_secs(20));
    let video = demux_until_quiet(&mut app, deadline, "app")
        .iter()
        .filter(|ev| {
            matches!(
                ev,
                tst_core::mpegts::demux::DemuxEvent::Sample { stream, .. } if stream.pid == 0x100
            )
        })
        .count();
    assert!(
        video > 10,
        "the app read past the first batch: {video} samples"
    );
    server.stop().unwrap();
}

/// A reader's TEARDOWN keys on the mount it set up and reads, whatever
/// its URI says: aimed at an unruled path, it still answers `/a`'s Refuse
/// status and ends the role, so the next TEARDOWN finds an idle session
/// keyed on its own (open) URI. Raw socket: `RtspClient::teardown`
/// clears its session whatever the status.
#[test]
fn a_readers_teardown_is_keyed_on_its_setup_mount() {
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
    rules.set_challenge(AuthScheme::Basic, "lab").unwrap();
    let port = server.local_addr().unwrap().port();

    let mut tcp = connect(port);
    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/a/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n{}\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n",
            basic("viewer", "pw")
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);

    rules
        .set(
            "/a",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::ServiceUnavailable),
            ),
        )
        .unwrap();
    let teardown = |tcp: &mut TcpStream, cseq: u32| {
        request(
            tcp,
            &format!(
                "TEARDOWN rtsp://127.0.0.1:{port}/other RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {sid}\r\n\r\n"
            ),
        )
    };
    let r = teardown(&mut tcp, 2);
    assert_eq!(status(&r), 503, "keyed on the SETUP mount: {r}");
    let r = teardown(&mut tcp, 3);
    assert_eq!(status(&r), 200, "the refused TEARDOWN ended the role: {r}");
    server.stop().unwrap();
}

/// A server with two muxer-backed mounts, `/open` (read `Open`) and
/// `/secret` (read `Refuse(NotFound)`); `/open`'s handle is returned.
fn open_and_secret() -> (RtspServer, tst_rtp::MountHandle) {
    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .unwrap()
        .build()
        .unwrap();
    let open = server.add_mount("/open", make_muxer_cfg()).unwrap();
    server.add_mount("/secret", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let rules = server.mount_rules();
    rules
        .set(
            "/open",
            MountRule::new(MountPolicy::Inherit, MountPolicy::Open),
        )
        .unwrap();
    rules
        .set(
            "/secret",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::NotFound),
            ),
        )
        .unwrap();
    (server, open)
}

/// A reader's interleaved SETUP of `path` (with `session` if given).
fn setup_reader(tcp: &mut TcpStream, port: u16, path: &str, cseq: u32, session: &str) -> String {
    let session = if session.is_empty() {
        String::new()
    } else {
        format!("Session: {session}\r\n")
    };
    request(
        tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}{path}/trackID=0 RTSP/1.0\r\nCSeq: {cseq}\r\n\
             {session}Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
}

/// What a TCP-interleaved reader's connection carries next.
enum Wire {
    /// A `$` frame on this channel.
    Frame(u8),
    /// An RTSP reply (no body).
    Reply(String),
    /// Nothing within the socket's read timeout.
    Quiet,
}

/// Reads `$` frames and RTSP replies off one connection, in order.
struct WireReader {
    buf: Vec<u8>,
}

impl WireReader {
    fn next(&mut self, tcp: &mut TcpStream) -> Wire {
        use std::io::Read as _;
        loop {
            if self.buf.first() == Some(&b'$') {
                if self.buf.len() >= 4 {
                    let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                    if self.buf.len() >= 4 + len {
                        let ch = self.buf[1];
                        self.buf.drain(..4 + len);
                        return Wire::Frame(ch);
                    }
                }
            } else if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let reply = String::from_utf8_lossy(&self.buf[..end + 4]).into_owned();
                self.buf.drain(..end + 4);
                return Wire::Reply(reply);
            }
            let mut chunk = [0u8; 4096];
            match tcp.read(&mut chunk) {
                Ok(0) => panic!("the server closed the connection"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => return Wire::Quiet,
            }
        }
    }

    /// The next RTSP reply, skipping media frames.
    fn reply(&mut self, tcp: &mut TcpStream) -> String {
        let deadline = std::time::Instant::now() + scaled(Duration::from_secs(5));
        while std::time::Instant::now() < deadline {
            if let Wire::Reply(r) = self.next(tcp) {
                return r;
            }
        }
        panic!("no reply within the deadline");
    }
}

/// A reader that has set up `/open` and SETUPs `/secret` is judged on the
/// mount the second SETUP names: it answers `/secret`'s Refuse status. The
/// refusal concerns another mount, so the reader keeps its role on `/open`
/// and can still PLAY it.
#[test]
fn a_ready_reader_keeps_its_role_after_a_refused_setup_elsewhere() {
    let (server, _open) = open_and_secret();
    let port = server.local_addr().unwrap().port();

    let mut tcp = connect(port);
    let r = setup_reader(&mut tcp, port, "/open", 1, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    let r = setup_reader(&mut tcp, port, "/secret", 2, &sid);
    assert_eq!(status(&r), 404, "keyed on /secret, not /open: {r}");
    let r = in_session(&mut tcp, port, "PLAY", "/open", 3, &sid);
    assert_eq!(status(&r), 200, "the reader kept its role on /open: {r}");
    server.stop().unwrap();
}

/// A playing reader's SETUP of a refused mount is answered with that
/// mount's status and leaves its stream running: media pushed after the
/// refusal still reaches it, and a PAUSE of its own mount is answered.
#[test]
fn a_playing_reader_keeps_its_stream_after_a_refused_setup_elsewhere() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tst_core::mpegts::common::Pts90khz;

    let (server, open) = open_and_secret();
    let port = server.local_addr().unwrap().port();

    let mut tcp = connect(port);
    let r = setup_reader(&mut tcp, port, "/open", 1, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    // The server picks the channel pair (from a counter shared by every
    // reader in the process), whatever pair the SETUP asked for.
    let rtp_channel: u8 = try_header(&r, "Transport")
        .and_then(|t| {
            t.split(';')
                .find_map(|p| p.trim().strip_prefix("interleaved="))
        })
        .and_then(|pair| pair.split('-').next())
        .and_then(|ch| ch.parse().ok())
        .unwrap_or_else(|| panic!("no interleaved pair granted: {r}"));
    let r = in_session(&mut tcp, port, "PLAY", "/open", 2, &sid);
    assert_eq!(status(&r), 200, "{r}");
    assert!(eventually(|| open.peer_count() == 1), "the reader plays");
    // Nothing has been pushed yet, so the next reply is the SETUP's.
    let r = setup_reader(&mut tcp, port, "/secret", 3, &sid);
    assert_eq!(status(&r), 404, "{r}");
    assert_eq!(open.peer_count(), 1, "the refusal touched no fanout");

    tcp.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut wire = WireReader { buf: Vec::new() };
    let stop = AtomicBool::new(false);
    let got_rtp = std::thread::scope(|s| {
        s.spawn(|| {
            let mut nal = vec![0u8; 1024];
            nal[..5].copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
            let mut pts = 0i64;
            while !stop.load(Ordering::Acquire) {
                open.push_video(&nal, Pts90khz::new(pts), true).unwrap();
                pts += 3600;
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let deadline = std::time::Instant::now() + scaled(Duration::from_secs(5));
        let mut seen = false;
        while !seen && std::time::Instant::now() < deadline {
            seen = matches!(wire.next(&mut tcp), Wire::Frame(ch) if ch == rtp_channel);
        }
        stop.store(true, Ordering::Release);
        seen
    });
    assert!(got_rtp, "the stream went on after the refused SETUP");

    use std::io::Write as _;
    tcp.write_all(
        format!("PAUSE rtsp://127.0.0.1:{port}/open RTSP/1.0\r\nCSeq: 4\r\nSession: {sid}\r\n\r\n")
            .as_bytes(),
    )
    .unwrap();
    let r = wire.reply(&mut tcp);
    assert_eq!(status(&r), 200, "{r}");
    server.stop().unwrap();
}

/// Behind a reader's own role, a SETUP or a DESCRIBE of a `Refuse(NotFound)`
/// mount and one of a path with no mount at all read alike, now and in what
/// follows.
#[test]
fn a_refused_setup_elsewhere_reads_like_a_missing_mount() {
    let (server, _open) = open_and_secret();
    let port = server.local_addr().unwrap().port();
    let statuses = |elsewhere: &str| {
        let mut tcp = connect(port);
        let r = setup_reader(&mut tcp, port, "/open", 1, "");
        let sid = session_id(&r);
        let mut seen = vec![status(&r)];
        seen.push(status(&setup_reader(&mut tcp, port, elsewhere, 2, &sid)));
        seen.push(status(&in_session(
            &mut tcp, port, "DESCRIBE", elsewhere, 3, &sid,
        )));
        seen.push(status(&in_session(
            &mut tcp, port, "PLAY", "/open", 4, &sid,
        )));
        seen.push(status(&in_session(
            &mut tcp, port, "PAUSE", "/open", 5, &sid,
        )));
        seen
    };
    let hidden = statuses("/secret");
    let missing = statuses("/nope");
    assert_eq!(
        hidden, missing,
        "a hidden mount told apart from a missing one"
    );
    assert_eq!(missing, [200, 404, 404, 200, 200]);
    server.stop().unwrap();
}

/// A publisher's DESCRIBE of a `Refuse(NotFound)` mount reads like one of a
/// path with no mount, and neither costs the publisher its announced mount:
/// its track SETUP is still served.
#[test]
fn a_publishers_refused_describe_elsewhere_keeps_its_mount() {
    let (server, _open) = open_and_secret();
    server.add_publish_mount("/pub-a").unwrap();
    server.add_publish_mount("/pub-b").unwrap();
    let port = server.local_addr().unwrap().port();
    let statuses = |own: &str, elsewhere: &str| {
        let mut p = RawPublisher::connect(port);
        let mut seen = vec![p.announce(own, SDP_MP2T)];
        let uri = p.uri(elsewhere);
        p.write_raw(format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 50\r\n\r\n").as_bytes());
        seen.push(status(&p.read_responses(1)));
        seen.push(p.setup_interleaved(own, "streamid=0").0);
        seen
    };
    // Each run announces a path of its own: the first connection's claim
    // is released only once the server has seen that connection close.
    let hidden = statuses("/pub-a", "/secret");
    let missing = statuses("/pub-b", "/nope");
    assert_eq!(
        hidden, missing,
        "a hidden mount told apart from a missing one"
    );
    assert_eq!(missing, [200, 404, 200]);
    server.stop().unwrap();
}

/// A refusal on the reader's own mount still ends its role: `/open` turns
/// to `Refuse` while a READY reader holds it, its track SETUP is answered
/// with the Refuse status, and once the rule opens again the old session is
/// gone.
#[test]
fn a_reader_refused_on_its_own_mount_loses_its_role() {
    let (server, _open) = open_and_secret();
    let port = server.local_addr().unwrap().port();
    let rules = server.mount_rules();

    let mut tcp = connect(port);
    let r = setup_reader(&mut tcp, port, "/open", 1, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    rules
        .set(
            "/open",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::Forbidden),
            ),
        )
        .unwrap();
    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/open/trackID=1 RTSP/1.0\r\nCSeq: 2\r\n\
             Session: {sid}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 403, "{r}");
    rules
        .set(
            "/open",
            MountRule::new(MountPolicy::Inherit, MountPolicy::Open),
        )
        .unwrap();
    let r = in_session(&mut tcp, port, "PLAY", "/open", 3, &sid);
    assert_eq!(status(&r), 454, "the refusal ended the reader's role: {r}");
    server.stop().unwrap();
}

/// What a `Refuse` rule does not hide: on an Idle connection, PLAY, PAUSE,
/// RECORD and TEARDOWN of a `Refuse(NotFound)` path answer the rule's
/// status, where the same requests for a path with no rule answer by the
/// session's state.
#[test]
fn an_idle_play_tells_a_refused_path_from_a_missing_one() {
    let (server, _open) = open_and_secret();
    let port = server.local_addr().unwrap().port();
    let answer = |method: &str, path: &str| {
        // One connection per request: an Idle TEARDOWN answered 200 closes it.
        let mut tcp = connect(port);
        status(&request(
            &mut tcp,
            &format!("{method} rtsp://127.0.0.1:{port}{path} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
        ))
    };
    for (method, missing) in [
        ("PLAY", 454),
        ("PAUSE", 454),
        ("RECORD", 455),
        ("TEARDOWN", 200),
    ] {
        assert_eq!(answer(method, "/secret"), 404, "{method} /secret");
        assert_eq!(answer(method, "/nope"), missing, "{method} /nope");
    }
    server.stop().unwrap();
}

/// What a `Refuse` rule does not hide, ANNOUNCE's half: the rule answers
/// before the handler reads the body or the session's state, so an ANNOUNCE
/// without a body reads the rule's status for a refused path and the
/// handler's for a path with no rule (400 from an Idle connection, 455 from
/// a reader's).
#[test]
fn an_announce_tells_a_refused_path_from_a_missing_one() {
    let (server, _open) = open_and_secret();
    let port = server.local_addr().unwrap().port();
    server
        .mount_rules()
        .set(
            "/secret",
            MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)),
        )
        .unwrap();
    let idle = |path: &str| {
        let mut tcp = connect(port);
        status(&request(
            &mut tcp,
            &format!("ANNOUNCE rtsp://127.0.0.1:{port}{path} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
        ))
    };
    let as_reader = |path: &str| {
        let mut tcp = connect(port);
        let r = setup_reader(&mut tcp, port, "/open", 1, "");
        assert_eq!(status(&r), 200, "{r}");
        let sid = session_id(&r);
        status(&in_session(&mut tcp, port, "ANNOUNCE", path, 2, &sid))
    };
    assert_eq!((idle("/secret"), idle("/nope")), (404, 400), "Idle");
    assert_eq!(
        (as_reader("/secret"), as_reader("/nope")),
        (404, 455),
        "a reader's connection"
    );
    server.stop().unwrap();
}

/// A refused SETUP on the reader's own mount ends its role and leaves the
/// connection open, and the connection can then set up an allowed mount
/// and play it: the RTP actually arrives, so the ended role left no
/// cancelled state behind.
#[test]
fn a_reader_refused_mid_session_can_play_another_mount_on_the_same_connection() {
    use std::io::Read as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tst_core::mpegts::common::Pts90khz;

    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .unwrap()
        .build()
        .unwrap();
    let open = server.add_mount("/open", make_muxer_cfg()).unwrap();
    server.add_mount("/secret", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let rules = server.mount_rules();
    rules
        .set(
            "/open",
            MountRule::new(MountPolicy::Inherit, MountPolicy::Open),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();

    let mut tcp = connect(port);
    let r = setup_reader(&mut tcp, port, "/secret", 1, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    rules
        .set(
            "/secret",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::NotFound),
            ),
        )
        .unwrap();
    let r = setup_reader(&mut tcp, port, "/secret", 2, &sid);
    assert_eq!(status(&r), 404, "the refused SETUP ends the role: {r}");

    let r = setup_reader(&mut tcp, port, "/open", 3, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    let r = request(
        &mut tcp,
        &format!("PLAY rtsp://127.0.0.1:{port}/open RTSP/1.0\r\nCSeq: 4\r\nSession: {sid}\r\n\r\n"),
    );
    assert_eq!(status(&r), 200, "{r}");

    let stop = AtomicBool::new(false);
    let got_rtp = std::thread::scope(|s| {
        s.spawn(|| {
            let mut nal = vec![0u8; 1024];
            nal[..5].copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
            let mut pts = 0i64;
            while !stop.load(Ordering::Acquire) {
                open.push_video(&nal, Pts90khz::new(pts), true).unwrap();
                pts += 3600;
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        tcp.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let deadline = std::time::Instant::now() + scaled(Duration::from_secs(5));
        let mut chunk = [0u8; 4096];
        let mut seen = false;
        while !seen && std::time::Instant::now() < deadline {
            match tcp.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => seen = chunk[..n].contains(&b'$'),
                Err(_) => {}
            }
        }
        stop.store(true, Ordering::Release);
        seen
    });
    assert!(got_rtp, "the second PLAY's fanout delivers interleaved RTP");
    server.stop().unwrap();
}

/// The publisher twin: a publisher refused at SETUP can ANNOUNCE another
/// mount on the same connection, and the media it records reaches it.
#[test]
fn a_publisher_refused_mid_session_can_publish_another_mount_on_the_same_connection() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    server.add_publish_mount("/amber").unwrap();
    let other = server.add_publish_mount("/other").unwrap();
    let port = server.local_addr().unwrap().port();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/amber", SDP_H264), 200);
    server
        .mount_rules()
        .set(
            "/amber",
            publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
    assert_eq!(p.setup_interleaved("/amber", "streamid=0").0, 403);

    assert_eq!(p.announce("/other", SDP_H264), 200);
    let (st, pair) = p.setup_interleaved_pair("/other", "streamid=0");
    assert_eq!(st, 200);
    let (rtp_ch, _) = pair.expect("server allocated an interleaved pair");
    assert_eq!(p.record_from_start("/other"), 200);
    for pkt in &h264_rtp_packets(10, PT, 100, SSRC, TS0) {
        p.send_frame(rtp_ch, pkt);
    }
    assert!(
        eventually(|| other.stats().aus_emitted >= 9),
        "recorded media reached the second mount: {} AUs",
        other.stats().aus_emitted
    );
    server.stop().unwrap();
}

/// A rule is set under the canonical key only, and governs every request
/// spelling that resolves to it: `set("/amber/")` is refused, and an
/// on-demand ANNOUNCE to `/amber/` (or `/amber`) meets the `/amber` rule's
/// refusal rather than the open default.
#[test]
fn a_canonical_rule_governs_the_announce_on_every_spelling() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.accept_unregistered_publishers(true);
    let server = started(b);
    assert!(matches!(
        server.mount_rules().set(
            "/amber/",
            publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        ),
        Err(MountRuleError::InvalidMountPath { .. })
    ));
    server
        .mount_rules()
        .set(
            "/amber",
            publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/amber/", SDP_MP2T), 403);
    assert_eq!(p.announce("/amber", SDP_MP2T), 403);
    assert_eq!(server.stats().mounts, 0, "no on-demand mount was created");
    server.stop().unwrap();
}

/// A rule belongs to the path, not the mount: removing the mount keeps it,
/// and a mount added again at the path is governed by it.
#[test]
fn remove_mount_keeps_the_rule_and_a_new_mount_inherits_it() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let rules = server.mount_rules();
    rules
        .set(
            "/amber",
            publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden)),
        )
        .unwrap();
    let _first = server.add_publish_mount("/amber").unwrap();
    server.remove_mount("/amber").unwrap();
    assert_eq!(rules.len(), 1);

    let mount = server.add_publish_mount("/amber").unwrap();
    let port = server.local_addr().unwrap().port();
    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/amber", SDP_MP2T), 403);
    assert!(mount.publisher().is_none());
    server.stop().unwrap();
}

/// Rule edits racing gated requests: one thread flips the publish rule
/// between Require and Refuse while four connections ANNOUNCE (and tear
/// down what they win). Every answer is 200, 401 or 403 (403 also covers
/// losing the slot to another connection), never a 500, and the server
/// still admits a publisher afterwards.
#[test]
fn rules_edited_while_clients_request_never_deadlock_or_tear() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    // Headroom over the default 64: each won ANNOUNCE costs a connection,
    // and a closed one is reaped asynchronously.
    b.max_sessions(1024);
    let server = started(b);
    let mount = server.add_publish_mount("/amber").unwrap();
    let rules = server.mount_rules();
    rules.set_challenge(AuthScheme::Basic, "lab").unwrap();
    let require = publish_rule(MountPolicy::Require(vec![cred("a", "pw")]));
    let refuse = publish_rule(MountPolicy::Refuse(RefuseStatus::Forbidden));
    rules.set("/amber", require.clone()).unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/amber");
    let auth = basic("a", "pw");

    let done = AtomicBool::new(false);
    let answered = AtomicUsize::new(0);
    let deadline = std::time::Instant::now() + scaled(Duration::from_secs(2));
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut flips = 0u32;
            while flips < 200 || !done.load(Ordering::Acquire) {
                let rule = if flips.is_multiple_of(2) {
                    &refuse
                } else {
                    &require
                };
                rules.set("/amber", rule.clone()).unwrap();
                flips += 1;
                std::thread::yield_now();
            }
        });
        let clients: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    let mut tcp = connect(port);
                    let mut cseq = 0;
                    while std::time::Instant::now() < deadline {
                        cseq += 1;
                        let r = announce(&mut tcp, &uri, cseq, &auth);
                        let st = status(&r);
                        assert!(matches!(st, 200 | 401 | 403), "ANNOUNCE: {r}");
                        answered.fetch_add(1, Ordering::Relaxed);
                        if st == 200 {
                            // Release the slot; a 200 TEARDOWN closes the
                            // connection, so start a fresh one either way.
                            cseq += 1;
                            let r = request(
                                &mut tcp,
                                &format!("TEARDOWN {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}\r\n"),
                            );
                            assert!(matches!(status(&r), 200 | 401 | 403), "TEARDOWN: {r}");
                            tcp = connect(port);
                            cseq = 0;
                        }
                    }
                })
            })
            .collect();
        for c in clients {
            let joined = c.join();
            done.store(true, Ordering::Release);
            joined.unwrap();
        }
        done.store(true, Ordering::Release);
    });
    assert!(answered.load(Ordering::Relaxed) > 0);

    rules.set("/amber", require).unwrap();
    // A slot held by a connection that is still closing frees shortly.
    let winner = std::cell::RefCell::new(None);
    let admitted = eventually(|| {
        let mut tcp = connect(port);
        let won = status(&announce(&mut tcp, &uri, 1, &auth)) == 200;
        if won {
            *winner.borrow_mut() = Some(tcp);
        }
        won
    });
    assert!(admitted, "the server admits a publisher after the edits");
    assert!(mount.publisher().is_some());
    drop(winner);
    server.stop().unwrap();
}

/// A hard cancel closes the rules table and refuses mount registration even
/// on a server that was never started.
#[test]
fn hard_cancel_on_a_never_started_server_closes_the_rules_table() {
    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .unwrap()
        .build()
        .unwrap();
    let rules = server.mount_rules();
    server.cancel_handle().cancel();
    assert!(rules.is_closed());
    assert!(matches!(
        rules.set("/a", MountRule::default()),
        Err(MountRuleError::Closed)
    ));
    assert!(matches!(
        server.add_publish_mount("/a"),
        Err(tst_rtp::RtspServerError::Shutdown)
    ));
}

/// A Digest-SHA-256 `Authorization` line answering `challenge`, as
/// [`digest_md5`].
fn digest_sha256(
    challenge: &str,
    nc: &Cell<u32>,
    user: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> String {
    let sha = |s: &str| -> String {
        sha2::Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let realm = challenge_param(challenge, "realm").unwrap();
    let nonce = challenge_param(challenge, "nonce").unwrap();
    nc.set(nc.get() + 1);
    let nc = format!("{:08x}", nc.get());
    let ha1 = sha(&format!("{user}:{realm}:{password}"));
    let ha2 = sha(&format!("{method}:{uri}"));
    let resp = sha(&format!("{ha1}:{nonce}:{nc}:cafebabe:auth:{ha2}"));
    format!(
        r#"Authorization: Digest username="{user}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{resp}", algorithm=SHA-256, qop=auth, nc={nc}, cnonce="cafebabe""#
    ) + "\r\n"
}

/// An `Authorization` line answering `challenge` in the scheme it offers.
fn answer(
    challenge: &str,
    nc: &Cell<u32>,
    user: &str,
    password: &str,
    method: &str,
    uri: &str,
) -> String {
    if challenge.starts_with("Basic") {
        basic(user, password)
    } else if challenge_param(challenge, "algorithm").as_deref() == Some("SHA-256") {
        digest_sha256(challenge, nc, user, password, method, uri)
    } else {
        digest_md5(challenge, nc, user, password, method, uri)
    }
}

/// On one fresh connection: `method` on `path` without a credential, then
/// (when challenged) answered with `user:password` in the scheme the
/// challenge offers. Returns the first status, the challenge without its
/// nonce (`-` when none) and the answered status (`-` when not challenged).
fn attempt(port: u16, method: &str, path: &str, user: &str, password: &str) -> String {
    let uri = format!("rtsp://127.0.0.1:{port}{path}");
    let send = |tcp: &mut TcpStream, cseq: u32, auth: &str| {
        if method == "ANNOUNCE" {
            announce(tcp, &uri, cseq, auth)
        } else {
            request(
                tcp,
                &format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}\r\n"),
            )
        }
    };
    let mut tcp = connect(port);
    let r = send(&mut tcp, 1, "");
    let Some(ch) = try_header(&r, "WWW-Authenticate").map(str::to_owned) else {
        return format!("{} [-] -", status(&r));
    };
    let shape = if ch.starts_with("Basic") {
        ch.clone()
    } else {
        format!(
            "Digest realm={:?} algorithm={}",
            challenge_param(&ch, "realm").unwrap_or_default(),
            challenge_param(&ch, "algorithm").unwrap_or_default()
        )
    };
    let nc = Cell::new(0);
    let answered = send(&mut tcp, 2, &answer(&ch, &nc, user, password, method, &uri));
    format!("{} [{shape}] {}", status(&r), status(&answered))
}

/// A started server with local mounts `/a` and `/b` and publish mounts
/// `/pub1` to `/pub6`, three for each of two passes of [`class_answers`].
fn served_for_classes(b: RtspServerBuilder) -> RtspServer {
    let server = b.build().unwrap();
    server.add_mount("/a", make_muxer_cfg()).unwrap();
    server.add_mount("/b", make_muxer_cfg()).unwrap();
    for i in 1..=6 {
        server.add_publish_mount(&format!("/pub{i}")).unwrap();
    }
    server.start().unwrap();
    server
}

/// What one class of server answers: a DESCRIBE of `/a` and of `/b` and an
/// ANNOUNCE (each on a publish mount of its own, as a won ANNOUNCE holds its
/// slot), each without a credential and then with `admin:secret`,
/// `viewer:pw` and `admin:WRONG`. A second `pass` on the same server
/// announces on mounts the first pass never used: a first-pass publisher
/// holds its mount until the server has reaped its closed connection, which
/// another thread does at a time of its own.
fn class_answers(server: &RtspServer, pass: u32) -> Vec<String> {
    let port = server.local_addr().unwrap().port();
    let mut lines = Vec::new();
    for (method, base) in [("DESCRIBE", "/a"), ("DESCRIBE", "/b"), ("ANNOUNCE", "/pub")] {
        let path = |i: u32| {
            if method == "ANNOUNCE" {
                format!("{base}{}", 3 * pass + i)
            } else {
                base.to_string()
            }
        };
        let admin = attempt(port, method, &path(1), "admin", "secret");
        let viewer = attempt(port, method, &path(2), "viewer", "pw");
        let wrong = attempt(port, method, &path(3), "admin", "WRONG");
        let last = |s: &str| s.rsplit(' ').next().unwrap().to_owned();
        lines.push(format!(
            "{method} {base} {admin} viewer={} wrong={}",
            last(&viewer),
            last(&wrong)
        ));
    }
    lines
}

fn builder_basic() -> RtspServerBuilder {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_basic("realm", "admin", pw("secret"));
    b
}

fn require_viewer() -> MountRule {
    MountRule::both(MountPolicy::Require(vec![cred("viewer", "pw")]))
}

/// On a server built with a credential, `set_challenge` sets the challenge
/// of every 401, and both the server's own credential (on an `Inherit`
/// path) and a rule's (on a `Require` path) are checked in it.
#[test]
fn set_challenge_takes_effect_on_a_server_built_with_a_credential() {
    let server = served_for_classes(builder_basic());
    let rules = server.mount_rules();
    rules.set("/b", require_viewer()).unwrap();
    rules
        .set_challenge(AuthScheme::DigestSha256, "lab")
        .unwrap();
    assert_eq!(
        class_answers(&server, 0),
        [
            r#"DESCRIBE /a 401 [Digest realm="lab" algorithm=SHA-256] 200 viewer=401 wrong=401"#,
            r#"DESCRIBE /b 401 [Digest realm="lab" algorithm=SHA-256] 401 viewer=200 wrong=401"#,
            r#"ANNOUNCE /pub 401 [Digest realm="lab" algorithm=SHA-256] 200 viewer=401 wrong=401"#,
        ]
    );
    server.stop().unwrap();
}

/// The twin: a server built with a Digest credential, `set_challenge` to
/// Basic.
#[test]
fn set_challenge_takes_effect_on_a_server_built_with_a_credential_from_digest_to_basic() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_md5("realm", "admin", pw("secret"));
    let server = served_for_classes(b);
    let rules = server.mount_rules();
    rules.set("/b", require_viewer()).unwrap();
    rules.set_challenge(AuthScheme::Basic, "lab").unwrap();
    assert_eq!(
        class_answers(&server, 0),
        [
            r#"DESCRIBE /a 401 [Basic realm="lab"] 200 viewer=401 wrong=401"#,
            r#"DESCRIBE /b 401 [Basic realm="lab"] 401 viewer=200 wrong=401"#,
            r#"ANNOUNCE /pub 401 [Basic realm="lab"] 200 viewer=401 wrong=401"#,
        ]
    );
    server.stop().unwrap();
}

/// Every class of credential configuration that `set_challenge` taking
/// effect must not move answers as it did before.
#[test]
fn the_measured_credential_classes_are_unchanged() {
    const OPEN: [&str; 3] = [
        "DESCRIBE /a 200 [-] - viewer=- wrong=-",
        "DESCRIBE /b 200 [-] - viewer=- wrong=-",
        "ANNOUNCE /pub 200 [-] - viewer=- wrong=-",
    ];
    let builder_only = |ch: &str| {
        [
            format!("DESCRIBE /a 401 [{ch}] 200 viewer=401 wrong=401"),
            format!("DESCRIBE /b 401 [{ch}] 200 viewer=401 wrong=401"),
            format!("ANNOUNCE /pub 401 [{ch}] 200 viewer=401 wrong=401"),
        ]
    };
    let basic_realm = r#"Basic realm="realm""#;
    let check = |class: &str, server: RtspServer, want: &[String]| {
        assert_eq!(class_answers(&server, 0), want, "{class}");
        server.stop().unwrap();
    };
    let owned = |rows: [&str; 3]| rows.map(str::to_owned);

    // A builder credential alone, each scheme.
    check(
        "builder Basic",
        served_for_classes(builder_basic()),
        &builder_only(basic_realm),
    );
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_md5("realm", "admin", pw("secret"));
    check(
        "builder Digest MD5",
        served_for_classes(b),
        &builder_only(r#"Digest realm="realm" algorithm=MD5"#),
    );
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_sha256("realm", "admin", pw("secret"));
    check(
        "builder Digest SHA-256",
        served_for_classes(b),
        &builder_only(r#"Digest realm="realm" algorithm=SHA-256"#),
    );

    // No builder credential; `/b` requires a rule credential,
    // with the table's challenge and after `set_challenge`.
    let s = served_for_classes(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    s.mount_rules().set("/b", require_viewer()).unwrap();
    let k2 = |ch: &str| {
        [
            OPEN[0].to_owned(),
            format!("DESCRIBE /b 401 [{ch}] 401 viewer=200 wrong=401"),
            OPEN[2].to_owned(),
        ]
    };
    assert_eq!(
        class_answers(&s, 0),
        k2(r#"Digest realm="tstrans" algorithm=MD5"#),
        "a Require path, the table's first challenge"
    );
    s.mount_rules()
        .set_challenge(AuthScheme::Basic, "lab")
        .unwrap();
    assert_eq!(
        class_answers(&s, 1),
        k2(r#"Basic realm="lab""#),
        "a Require path after set_challenge"
    );
    s.stop().unwrap();

    // No builder credential; the default rule requires.
    let s = served_for_classes(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    s.mount_rules().set_default(require_viewer()).unwrap();
    let ch = r#"Digest realm="tstrans" algorithm=MD5"#;
    check(
        "a Require default",
        s,
        &[
            format!("DESCRIBE /a 401 [{ch}] 401 viewer=200 wrong=401"),
            format!("DESCRIBE /b 401 [{ch}] 401 viewer=200 wrong=401"),
            format!("ANNOUNCE /pub 401 [{ch}] 401 viewer=200 wrong=401"),
        ],
    );

    // A builder credential and a `Require` path, Digest and Basic.
    for (class, ch, b) in [
        (
            "builder Digest MD5 and a Require path",
            r#"Digest realm="realm" algorithm=MD5"#,
            {
                let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
                b.auth_digest_md5("realm", "admin", pw("secret"));
                b
            },
        ),
        (
            "builder Basic and a Require path",
            basic_realm,
            builder_basic(),
        ),
    ] {
        let s = served_for_classes(b);
        s.mount_rules().set("/b", require_viewer()).unwrap();
        check(
            class,
            s,
            &[
                format!("DESCRIBE /a 401 [{ch}] 200 viewer=401 wrong=401"),
                format!("DESCRIBE /b 401 [{ch}] 401 viewer=200 wrong=401"),
                format!("ANNOUNCE /pub 401 [{ch}] 200 viewer=401 wrong=401"),
            ],
        );
    }

    // Nothing requires, `set_challenge` called anyway.
    let s = served_for_classes(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    s.mount_rules()
        .set_challenge(AuthScheme::Basic, "lab")
        .unwrap();
    check("set_challenge with nothing that requires", s, &owned(OPEN));

    // A builder credential and a builder default rule of `Open`.
    let mut b = builder_basic();
    b.default_mount_rule(MountRule::both(MountPolicy::Open));
    check(
        "builder credential, builder default Open",
        served_for_classes(b),
        &owned(OPEN),
    );

    // A builder credential, a builder default rule that requires a rule
    // credential, and `/b` inheriting it.
    let mut b = builder_basic();
    b.default_mount_rule(require_viewer());
    let s = served_for_classes(b);
    s.mount_rules().set("/b", MountRule::default()).unwrap();
    check(
        "builder credential, builder default Require",
        s,
        &[
            format!("DESCRIBE /a 401 [{basic_realm}] 401 viewer=200 wrong=401"),
            format!("DESCRIBE /b 401 [{basic_realm}] 401 viewer=200 wrong=401"),
            format!("ANNOUNCE /pub 401 [{basic_realm}] 401 viewer=200 wrong=401"),
        ],
    );

    // A runtime default of `Open`, then reset.
    let s = served_for_classes(builder_basic());
    s.mount_rules()
        .set_default(MountRule::both(MountPolicy::Open))
        .unwrap();
    assert_eq!(
        class_answers(&s, 0),
        owned(OPEN),
        "builder credential, runtime default Open"
    );
    // A runtime reset of the default rule still requires the server's
    // credential.
    s.mount_rules().set_default(MountRule::default()).unwrap();
    assert_eq!(
        class_answers(&s, 1),
        builder_only(basic_realm),
        "builder credential, runtime default reset"
    );
    s.stop().unwrap();

    // A builder credential, a runtime default of `Refuse(403)`, `/b`
    // open.
    let s = served_for_classes(builder_basic());
    s.mount_rules()
        .set_default(MountRule::both(MountPolicy::Refuse(
            RefuseStatus::Forbidden,
        )))
        .unwrap();
    s.mount_rules()
        .set("/b", MountRule::both(MountPolicy::Open))
        .unwrap();
    check(
        "builder credential, runtime default Refuse, /b Open",
        s,
        &owned([
            "DESCRIBE /a 403 [-] - viewer=- wrong=-",
            "DESCRIBE /b 200 [-] - viewer=- wrong=-",
            "ANNOUNCE /pub 403 [-] - viewer=- wrong=-",
        ]),
    );

    // A builder credential, `/b` read-open and publish-inheriting.
    let s = served_for_classes(builder_basic());
    s.mount_rules()
        .set(
            "/b",
            MountRule::new(MountPolicy::Inherit, MountPolicy::Open),
        )
        .unwrap();
    let mut want = builder_only(basic_realm);
    want[1] = OPEN[1].to_owned();
    check("builder credential, /b read Open", s, &want);

    // No credential anywhere.
    check(
        "no credential",
        served_for_classes(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap()),
        &owned(OPEN),
    );
}

/// The requests a rule refused for their credential are counted on it, in
/// their direction, before each reply is written: a bare first challenge
/// is not, two wrong passwords are, and the third 401 closes the
/// connection.
#[test]
fn refused_answers_on_one_connection_are_counted_on_the_rule() {
    use tst_rtp::rtsp::server::mount_rules::MountRuleAuthFailures;
    let server = started_with_mount(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let rules = server.mount_rules();
    rules
        .set(
            "/a",
            MountRule::both(MountPolicy::Require(vec![cred("viewer", "pw")])),
        )
        .unwrap();
    rules.set_challenge(AuthScheme::Basic, "lab").unwrap();
    let port = server.local_addr().unwrap().port();
    let describe = |tcp: &mut TcpStream, cseq: u32, auth: &str| {
        request(
            tcp,
            &format!("DESCRIBE rtsp://127.0.0.1:{port}/a RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}\r\n"),
        )
    };
    let count = || rules.auth_failures("/a").map(|c| c.read);

    let mut tcp = connect(port);
    assert_eq!(status(&describe(&mut tcp, 1, "")), 401);
    assert_eq!(count(), Some(0), "a bare challenge is not counted");
    assert_eq!(
        status(&describe(&mut tcp, 2, &basic("viewer", "nope"))),
        401
    );
    assert_eq!(count(), Some(1));
    assert_eq!(
        status(&describe(&mut tcp, 3, &basic("viewer", "nope"))),
        401
    );
    assert_eq!(count(), Some(2));
    assert_eq!(rules.auth_failures("/a").unwrap().publish, 0);
    // The third 401 in a row closed the connection.
    use std::io::Read as _;
    let mut rest = [0u8; 16];
    let closed = match tcp.read(&mut rest) {
        Ok(n) => n == 0,
        // Windows reports the server's close as an abort.
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
    };
    assert!(closed, "the connection is closed");
    assert_eq!(
        rules.default_auth_failures(),
        MountRuleAuthFailures::default()
    );
    server.stop().unwrap();
}

/// A Digest answer computed for the challenge in force before
/// `set_challenge` changed it is refused on a server built with a
/// credential: a 401 that offers the new challenge without `stale`, and an
/// answer in the new challenge on the same connection is admitted.
#[test]
fn an_answer_for_the_old_challenge_is_refused_after_set_challenge() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_md5("realm", "admin", pw("secret"));
    let server = started_with_mount(b);
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/a");
    let describe = |tcp: &mut TcpStream, cseq: u32, auth: &str| {
        request(
            tcp,
            &format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}\r\n"),
        )
    };
    let nc = Cell::new(0);
    let mut tcp = connect(port);
    let r = describe(&mut tcp, 1, "");
    assert_eq!(status(&r), 401, "{r}");
    let old = try_header(&r, "WWW-Authenticate").unwrap().to_owned();
    assert_eq!(challenge_param(&old, "realm").as_deref(), Some("realm"));

    server
        .mount_rules()
        .set_challenge(AuthScheme::DigestSha256, "lab")
        .unwrap();
    let r = describe(
        &mut tcp,
        2,
        &digest_md5(&old, &nc, "admin", "secret", "DESCRIBE", &uri),
    );
    assert_eq!(status(&r), 401, "an answer for the old challenge: {r}");
    let new = try_header(&r, "WWW-Authenticate").unwrap().to_owned();
    assert_eq!(
        challenge_param(&new, "realm").as_deref(),
        Some("lab"),
        "{new}"
    );
    assert_eq!(
        challenge_param(&new, "algorithm").as_deref(),
        Some("SHA-256"),
        "{new}"
    );
    assert_eq!(challenge_param(&new, "stale"), None, "{new}");
    assert_eq!(
        challenge_param(&new, "nonce"),
        challenge_param(&old, "nonce"),
        "not re-challenged as stale"
    );

    let r = describe(
        &mut tcp,
        3,
        &digest_sha256(&new, &nc, "admin", "secret", "DESCRIBE", &uri),
    );
    assert_eq!(status(&r), 200, "an answer in the new challenge: {r}");
    server.stop().unwrap();
}

/// A Digest MD5 `Authorization` line under a nonce no connection was
/// offered.
fn stale_digest_md5(realm: &str, user: &str, password: &str, method: &str, uri: &str) -> String {
    let never_issued = format!("Digest realm=\"{realm}\", nonce=\"never-issued\"");
    digest_md5(&never_issued, &Cell::new(0), user, password, method, uri)
}

/// The challenge of a 401.
fn challenge_of(r: &str) -> String {
    assert_eq!(status(r), 401, "{r}");
    try_header(r, "WWW-Authenticate")
        .expect("a challenge")
        .to_owned()
}

/// A Digest answer under a nonce the connection was not offered is
/// challenged again with a new nonce and `stale=true`, the same reply
/// whatever user and password it names, and that 401 is no strike: four in
/// a row leave the connection open. It clears no strike either: with a
/// stale answer after each of the first two, the third refused password
/// still closes the connection. The same for the server's own credential
/// and for a rule's, whose refused-credential count leaves the stale
/// answers out.
#[test]
fn a_stale_nonce_is_challenged_with_stale_true_and_is_no_strike() {
    for on_rule in [false, true] {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
        let (realm, user, password) = if on_rule {
            ("tstrans", "viewer", "pw")
        } else {
            b.auth_digest_md5("realm", "admin", pw("secret"));
            ("realm", "admin", "secret")
        };
        let server = started_with_mount(b);
        if on_rule {
            server
                .mount_rules()
                .set(
                    "/a",
                    MountRule::both(MountPolicy::Require(vec![cred(user, password)])),
                )
                .unwrap();
        }
        let port = server.local_addr().unwrap().port();
        let uri = format!("rtsp://127.0.0.1:{port}/a");
        let describe = |tcp: &mut TcpStream, auth: &str| {
            request(
                tcp,
                &format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 1\r\n{auth}\r\n"),
            )
        };
        let stale =
            |user: &str, password: &str| stale_digest_md5(realm, user, password, "DESCRIBE", &uri);

        let mut tcp = connect(port);
        let mut challenge = String::new();
        let mut replies = Vec::new();
        for (u, p) in [
            (user, password),
            (user, "nope"),
            ("nobody", password),
            ("", password),
        ] {
            let r = describe(&mut tcp, &stale(u, p));
            let next = challenge_of(&r);
            let nonce = challenge_param(&next, "nonce").unwrap();
            assert_eq!(
                next,
                format!(
                    "Digest realm=\"{realm}\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\", stale=true"
                ),
                "on_rule={on_rule}, user {u:?}"
            );
            assert_ne!(
                challenge_param(&challenge, "nonce"),
                Some(nonce.clone()),
                "on_rule={on_rule}, user {u:?}: the nonce was offered before"
            );
            // The server writes its headers in no fixed order.
            let mut lines: Vec<String> = r.lines().map(|l| l.replace(&nonce, "N")).collect();
            lines.sort();
            replies.push(lines);
            challenge = next;
        }
        assert!(
            replies.iter().all(|r| r == &replies[0]),
            "on_rule={on_rule}: the stale reply depends on the credential: {replies:#?}"
        );

        for strike in 1..=3 {
            let wrong = digest_md5(&challenge, &Cell::new(0), user, "nope", "DESCRIBE", &uri);
            let refused = challenge_of(&describe(&mut tcp, &wrong));
            assert_eq!(
                refused,
                challenge.trim_end_matches(", stale=true"),
                "on_rule={on_rule}, strike {strike}: a refused password keeps the nonce and is not stale"
            );
            if strike < 3 {
                challenge = challenge_of(&describe(&mut tcp, &stale(user, password)));
                assert!(challenge.ends_with(", stale=true"), "{challenge}");
            }
        }
        use std::io::Read as _;
        let mut rest = [0u8; 16];
        let closed = match tcp.read(&mut rest) {
            Ok(n) => n == 0,
            // Windows reports the server's close as an abort.
            Err(e) => matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ),
        };
        assert!(
            closed,
            "on_rule={on_rule}: the third refused password did not close the connection"
        );
        if on_rule {
            assert_eq!(
                server.mount_rules().auth_failures("/a").map(|c| c.read),
                Some(3),
                "a stale answer is not a refused credential"
            );
        }
        server.stop().unwrap();
    }
}

/// What RFC 7616 asks of a client that reads `stale=true` works at any
/// point: it answers again under the new nonce with the same credential
/// and is admitted. A reader keeps its role through four stale PLAYs.
#[test]
fn an_answer_to_the_stale_challenge_is_admitted_and_a_reader_keeps_its_role() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_md5("realm", "admin", pw("secret"));
    let server = started_with_mount(b);
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/a");
    let track = format!("{uri}/trackID=0");
    let setup = |tcp: &mut TcpStream, auth: &str| {
        request(
            tcp,
            &format!(
                "SETUP {track} RTSP/1.0\r\nCSeq: 1\r\n{auth}Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
            ),
        )
    };
    let play = |tcp: &mut TcpStream, sid: &str, auth: &str| {
        request(
            tcp,
            &format!("PLAY {uri} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n{auth}\r\n"),
        )
    };

    // The connection's first request already answers under another nonce.
    let mut tcp = connect(port);
    let stale = stale_digest_md5("realm", "admin", "secret", "SETUP", &track);
    let challenge = challenge_of(&setup(&mut tcp, &stale));
    assert!(challenge.ends_with(", stale=true"), "{challenge}");
    let answer = digest_md5(
        &challenge,
        &Cell::new(0),
        "admin",
        "secret",
        "SETUP",
        &track,
    );
    let r = setup(&mut tcp, &answer);
    assert_eq!(status(&r), 200, "the answer under the new nonce: {r}");
    let sid = session_id(&r);

    let mut challenge = String::new();
    for _ in 0..4 {
        let stale = stale_digest_md5("realm", "admin", "secret", "PLAY", &uri);
        challenge = challenge_of(&play(&mut tcp, &sid, &stale));
        assert!(challenge.ends_with(", stale=true"), "{challenge}");
    }
    let answer = digest_md5(&challenge, &Cell::new(0), "admin", "secret", "PLAY", &uri);
    let r = play(&mut tcp, &sid, &answer);
    assert_eq!(
        status(&r),
        200,
        "the reader plays after four stale PLAYs: {r}"
    );
    drop(tcp);
    server.stop().unwrap();
}

/// The publish side: ANNOUNCE and RECORD under another nonce are challenged
/// as stale, four times each, none a strike, and the publisher's answer
/// under the new nonce is admitted.
#[test]
fn a_publishers_stale_nonce_is_challenged_with_stale_true_and_is_no_strike() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let mount = server.add_publish_mount("/amber").unwrap();
    server
        .mount_rules()
        .set(
            "/amber",
            publish_rule(MountPolicy::Require(vec![cred("amber", "pw")])),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/amber");
    let track = format!("{uri}/streamid=0");
    let stale = |method: &str| stale_digest_md5("tstrans", "amber", "pw", method, &uri);

    let mut tcp = connect(port);
    let mut challenge = String::new();
    for _ in 0..4 {
        challenge = challenge_of(&announce(&mut tcp, &uri, 1, &stale("ANNOUNCE")));
        assert!(challenge.ends_with(", stale=true"), "{challenge}");
        assert!(mount.publisher().is_none());
    }
    let nc = Cell::new(0);
    let answer = digest_md5(&challenge, &nc, "amber", "pw", "ANNOUNCE", &uri);
    let r = announce(&mut tcp, &uri, 2, &answer);
    assert_eq!(status(&r), 200, "{r}");
    let answer = digest_md5(&challenge, &nc, "amber", "pw", "SETUP", &track);
    let r = setup_record(&mut tcp, &track, 3, &answer);
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    let record = |tcp: &mut TcpStream, auth: &str| {
        request(
            tcp,
            &format!("RECORD {uri} RTSP/1.0\r\nCSeq: 4\r\nSession: {sid}\r\n{auth}\r\n"),
        )
    };
    for _ in 0..4 {
        challenge = challenge_of(&record(&mut tcp, &stale("RECORD")));
        assert!(challenge.ends_with(", stale=true"), "{challenge}");
    }
    let answer = digest_md5(&challenge, &Cell::new(0), "amber", "pw", "RECORD", &uri);
    let r = record(&mut tcp, &answer);
    assert_eq!(
        status(&r),
        200,
        "the publisher records after four stale RECORDs: {r}"
    );
    assert!(mount.publisher().is_some());
    drop(tcp);
    server.stop().unwrap();
}
