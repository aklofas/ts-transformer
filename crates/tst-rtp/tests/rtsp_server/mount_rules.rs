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

use crate::fixtures::raw_rtsp::{make_muxer_cfg, request, scaled, session_id, try_header};
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

/// A reader that has set up one mount and SETUPs another is judged on the
/// mount the second SETUP names, not the one it is leaving: a refused
/// second SETUP answers the Refuse status and ends the reader's role.
#[test]
fn a_readers_second_setup_is_keyed_on_the_mount_it_names() {
    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .unwrap()
        .build()
        .unwrap();
    server.add_mount("/open", make_muxer_cfg()).unwrap();
    let secret = server.add_mount("/secret", make_muxer_cfg()).unwrap();
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
    let port = server.local_addr().unwrap().port();

    let mut tcp = connect(port);
    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/open/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);

    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/secret/trackID=0 RTSP/1.0\r\nCSeq: 2\r\n\
             Session: {sid}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 404, "keyed on /secret, not /open: {r}");

    // A PLAY on the open mount it first set up finds no session: the
    // refused SETUP ended the role rather than leaving it on /open.

    let r = request(
        &mut tcp,
        &format!("PLAY rtsp://127.0.0.1:{port}/open RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n"),
    );
    assert_eq!(
        status(&r),
        454,
        "the refused SETUP ended the reader's role: {r}"
    );
    assert_eq!(secret.peer_count(), 0);
    server.stop().unwrap();
}

/// A refused SETUP that ends a reader's role leaves the connection open, and
/// the connection can then set up an allowed mount and play it: the RTP
/// actually arrives, so the ended role left no cancelled state behind.
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
    rules
        .set(
            "/secret",
            MountRule::new(
                MountPolicy::Inherit,
                MountPolicy::Refuse(RefuseStatus::NotFound),
            ),
        )
        .unwrap();
    let port = server.local_addr().unwrap().port();
    let setup = |tcp: &mut TcpStream, path: &str, cseq: u32, session: &str| {
        request(
            tcp,
            &format!(
                "SETUP rtsp://127.0.0.1:{port}{path}/trackID=0 RTSP/1.0\r\nCSeq: {cseq}\r\n\
                 {session}Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
            ),
        )
    };

    let mut tcp = connect(port);
    let r = setup(&mut tcp, "/open", 1, "");
    assert_eq!(status(&r), 200, "{r}");
    let sid = session_id(&r);
    let r = setup(&mut tcp, "/secret", 2, &format!("Session: {sid}\r\n"));
    assert_eq!(status(&r), 404, "the refused SETUP ends the role: {r}");

    let r = setup(&mut tcp, "/open", 3, "");
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

/// A rule set at a path with a trailing slash governs the mount requests
/// resolve that path to: an on-demand ANNOUNCE to `/amber/` (or `/amber`)
/// meets the `/amber/` rule's refusal rather than the open default.
#[test]
fn a_rule_set_with_a_trailing_slash_governs_the_announce() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.accept_unregistered_publishers(true);
    let server = started(b);
    server
        .mount_rules()
        .set(
            "/amber/",
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
