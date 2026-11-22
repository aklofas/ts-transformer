//! The server's credential rules as one table: which client form a server
//! credential admits, for every challenge scheme, whether the credential came
//! from the builder or from a `Require` rule, and what malformed
//! `Authorization` headers are answered.
//!
//! Every cell is a statement about the server as it is; a cell that changes is
//! a change in what the server admits.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine;
use md5::{Digest, Md5};
use secrecy::SecretString;
use tst_rtp::{
    AuthScheme, MountCredential, MountPolicy, MountRule, MountRuleError, RtspClient,
    RtspClientBuilder, RtspError, RtspServer, RtspServerBuilder, RtspServerError,
};

use crate::fixtures::raw_rtsp::{header, make_muxer_cfg, try_header};

const REALM: &str = "table";

fn pw(s: &str) -> SecretString {
    SecretString::new(s.into())
}

/// How the server was given its credential.
#[derive(Clone, Copy, Debug)]
enum Source {
    Builder,
    Rule,
}

const SOURCES: [Source; 2] = [Source::Builder, Source::Rule];
const SCHEMES: [AuthScheme; 3] = [
    AuthScheme::Basic,
    AuthScheme::DigestMd5,
    AuthScheme::DigestSha256,
];

/// A started server whose `/live` needs (`user`, `pass`).
fn server(source: Source, scheme: AuthScheme, user: &str, pass: &str) -> RtspServer {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
    if matches!(source, Source::Builder) {
        match scheme {
            AuthScheme::Basic => b.auth_basic(REALM, user, pw(pass)),
            AuthScheme::DigestMd5 => b.auth_digest_md5(REALM, user, pw(pass)),
            _ => b.auth_digest_sha256(REALM, user, pw(pass)),
        };
    }
    let server = b.build().expect("server build");
    if matches!(source, Source::Rule) {
        let rules = server.mount_rules();
        rules.set_challenge(scheme, REALM).expect("set_challenge");
        rules
            .set(
                "/live",
                MountRule::both(MountPolicy::Require(vec![MountCredential::new(
                    user,
                    pw(pass),
                )])),
            )
            .expect("rules.set");
    }
    server
        .add_mount("/live", make_muxer_cfg())
        .expect("add_mount");
    server.start().expect("server start");
    server
}

/// DESCRIBE's outcome: admitted, or refused with 401 twice (the client's
/// one retry). Any other error is a failure of the table, not a cell.
fn admitted(result: Result<(), RtspError>) -> bool {
    match result {
        Ok(()) => true,
        Err(RtspError::AuthFailed) => false,
        Err(e) => panic!("neither admitted nor refused with 401: {e:?}"),
    }
}

fn describe_url(url: &str) -> bool {
    let mut client = RtspClient::connect(url).expect("connect");
    admitted(client.describe().map(|_| ()))
}

fn describe_with(url: &str, user: &str, pass: &str) -> bool {
    let mut client = RtspClientBuilder::new(url)
        .expect("URL parse")
        .auth(user, pw(pass))
        .connect()
        .expect("connect");
    admitted(client.describe().map(|_| ()))
}

/// The ten client forms, in order C1 to C10:
/// C1 the URL's user info is the server's pair (percent-encoded `matching`),
/// C2 `admin:`, C3 `admin@`, C4 no credentials, C5 `:secret`,
/// C6 `admin:secret`, C7 `admin:wrong`,
/// C8 a URL pair for another user plus `.auth(` the server's pair `)`,
/// C9 the matching URL plus `.auth("admin", "")`,
/// C10 no user info plus `.auth("", "secret")`.
fn ten_forms(server: &RtspServer, user: &str, pass: &str, matching: &str) -> [bool; 10] {
    let port = server.local_addr().expect("local_addr").port();
    let host = format!("127.0.0.1:{port}/live");
    [
        describe_url(&format!("rtsp://{matching}@{host}")),
        describe_url(&format!("rtsp://admin:@{host}")),
        describe_url(&format!("rtsp://admin@{host}")),
        describe_url(&format!("rtsp://{host}")),
        describe_url(&format!("rtsp://:secret@{host}")),
        describe_url(&format!("rtsp://admin:secret@{host}")),
        describe_url(&format!("rtsp://admin:wrong@{host}")),
        describe_with(&format!("rtsp://other:creds@{host}"), user, pass),
        describe_with(&format!("rtsp://{matching}@{host}"), "admin", ""),
        describe_with(&format!("rtsp://{host}"), "", "secret"),
    ]
}

/// One server credential and what it admits: `admits` lists the forms (by
/// index, C1 = 0) that read 200; every other form reads 401.
struct Row {
    name: &'static str,
    user: &'static str,
    pass: &'static str,
    /// The user info that names this pair in a URL, percent-encoded.
    matching: &'static str,
    admits: &'static [usize],
    /// Not every scheme can carry every pair: a user name with a colon cannot
    /// authenticate under Basic (the server takes the first colon of the
    /// decoded pair as the separator), so that row is a Digest-only row.
    basic: bool,
}

const ROWS: &[Row] = &[
    Row {
        name: "S1 user and password",
        user: "admin",
        pass: "secret",
        matching: "admin:secret",
        admits: &[0, 5, 7],
        basic: true,
    },
    Row {
        name: "S2 user and an empty password",
        user: "admin",
        pass: "",
        matching: "admin:",
        admits: &[0, 1, 2, 7, 8],
        basic: true,
    },
    Row {
        name: "S3 percent-encoded user and password",
        user: "a b@c",
        pass: "p@ss/w:rd%?#",
        matching: "a%20b%40c:p%40ss%2Fw%3Ard%25%3F%23",
        admits: &[0, 7],
        basic: true,
    },
    Row {
        name: "S4 a colon in the user name (Digest only)",
        user: "a:b",
        pass: "x",
        matching: "a%3Ab:x",
        admits: &[0, 7],
        basic: false,
    },
    Row {
        name: "S5 UTF-8",
        user: "\u{e4}dmin",
        pass: "p\u{e4}ssw\u{f6}rd",
        matching: "%C3%A4dmin:p%C3%A4ssw%C3%B6rd",
        admits: &[0, 7],
        basic: true,
    },
    Row {
        name: "S7 quotes and backslashes",
        user: "a\"b\\c",
        pass: "p\"w\\",
        matching: "a%22b%5Cc:p%22w%5C",
        admits: &[0, 7],
        basic: true,
    },
];

fn expected(row: &Row) -> [bool; 10] {
    let mut cells = [false; 10];
    for &i in row.admits {
        cells[i] = true;
    }
    cells
}

fn cells_of(source: Source, scheme: AuthScheme, row: &Row) -> [bool; 10] {
    let server = server(source, scheme, row.user, row.pass);
    let cells = ten_forms(&server, row.user, row.pass, row.matching);
    server.stop().ok();
    cells
}

fn check_every_cell(source: Source) {
    for scheme in SCHEMES {
        for row in ROWS {
            if matches!(scheme, AuthScheme::Basic) && !row.basic {
                continue;
            }
            assert_eq!(
                cells_of(source, scheme, row),
                expected(row),
                "{source:?} / {scheme:?} / {}: C1 to C10 (true = 200)",
                row.name
            );
        }
    }
}

#[test]
fn the_builder_credential_admits_exactly_its_pair() {
    check_every_cell(Source::Builder);
}

#[test]
fn a_rule_credential_admits_exactly_its_pair() {
    check_every_cell(Source::Rule);
}

#[test]
fn the_builder_and_the_rule_path_agree_in_every_cell() {
    for scheme in SCHEMES {
        for row in ROWS {
            if matches!(scheme, AuthScheme::Basic) && !row.basic {
                continue;
            }
            assert_eq!(
                cells_of(Source::Builder, scheme, row),
                cells_of(Source::Rule, scheme, row),
                "{scheme:?} / {}: the two ways to configure one credential differ",
                row.name
            );
        }
    }
}

/// An empty password is a password: it admits its own user and nobody else,
/// and "no credentials" and an empty user name stay refused.
#[test]
fn an_empty_password_opens_only_for_its_user() {
    for source in SOURCES {
        for scheme in SCHEMES {
            let server = server(source, scheme, "admin", "");
            let port = server.local_addr().expect("local_addr").port();
            let host = format!("127.0.0.1:{port}/live");
            let what = format!("{source:?} / {scheme:?}");
            assert!(
                describe_url(&format!("rtsp://admin:@{host}")),
                "{what}: admin:"
            );
            assert!(
                describe_url(&format!("rtsp://admin@{host}")),
                "{what}: admin@"
            );
            assert!(!describe_url(&format!("rtsp://{host}")), "{what}: none");
            assert!(
                !describe_with(&format!("rtsp://{host}"), "", ""),
                "{what}: empty user"
            );
            assert!(
                !describe_url(&format!("rtsp://admin:secret@{host}")),
                "{what}: another password"
            );
            server.stop().ok();
        }
    }
}

/// A credential with no user name is refused where it is configured, so no
/// server ever holds one that could match an empty user name.
#[test]
fn an_empty_user_name_is_refused_where_it_is_configured() {
    for scheme in SCHEMES {
        let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").expect("URL parse");
        match scheme {
            AuthScheme::Basic => b.auth_basic(REALM, "", pw("secret")),
            AuthScheme::DigestMd5 => b.auth_digest_md5(REALM, "", pw("secret")),
            _ => b.auth_digest_sha256(REALM, "", pw("secret")),
        };
        assert!(
            matches!(b.build(), Err(RtspServerError::InvalidConfig { .. })),
            "{scheme:?}: build() takes an empty user name"
        );
    }
    let server = RtspServerBuilder::new("rtsp://127.0.0.1:0")
        .expect("URL parse")
        .build()
        .expect("server build");
    let rule = MountRule::both(MountPolicy::Require(vec![MountCredential::new(
        "",
        pw("secret"),
    )]));
    assert!(matches!(
        server.mount_rules().set("/live", rule.clone()),
        Err(MountRuleError::EmptyCredentials)
    ));
    assert!(matches!(
        server.mount_rules().set_default(rule),
        Err(MountRuleError::EmptyCredentials)
    ));
}

// ---- raw-socket rows --------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn connect(server: &RtspServer) -> TcpStream {
    let port = server.local_addr().expect("local_addr").port();
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    tcp
}

/// One request; the response head, or `None` when the server closed the
/// connection (or reset it) instead of answering.
fn try_request(tcp: &mut TcpStream, req: &str) -> Option<String> {
    tcp.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match tcp.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&buf).into_owned());
        }
    }
}

fn describe_request(port: u16, cseq: u32, authorization: Option<&str>) -> String {
    let auth = authorization
        .map(|a| format!("Authorization: {a}\r\n"))
        .unwrap_or_default();
    format!(
        "DESCRIBE rtsp://127.0.0.1:{port}/live RTSP/1.0\r\nCSeq: {cseq}\r\n{auth}Accept: application/sdp\r\n\r\n"
    )
}

fn status(response: &str) -> &str {
    response.lines().next().unwrap_or("")
}

/// A fresh connection, one DESCRIBE with `authorization`.
fn answer_to(server: &RtspServer, authorization: Option<&str>) -> String {
    let port = server.local_addr().expect("local_addr").port();
    let mut tcp = connect(server);
    try_request(&mut tcp, &describe_request(port, 1, authorization))
        .expect("the server answered the DESCRIBE")
}

fn assert_challenged(response: &str, scheme: &str, row: &str) {
    assert!(
        status(response).contains("401"),
        "{row}: expected 401, got {:?}",
        status(response)
    );
    let challenge = try_header(response, "WWW-Authenticate")
        .unwrap_or_else(|| panic!("{row}: a 401 without a challenge: {response:?}"));
    assert!(
        challenge.starts_with(scheme),
        "{row}: the challenge is {challenge:?}, expected {scheme}"
    );
    if scheme == "Digest" {
        assert!(
            challenge.contains("nonce=\""),
            "{row}: no nonce in {challenge:?}"
        );
    }
}

/// `key="value"` out of a header value.
fn param(value: &str, key: &str) -> String {
    let needle = format!("{key}=\"");
    let start = value.find(&needle).expect("parameter present") + needle.len();
    let end = value[start..].find('"').expect("closing quote") + start;
    value[start..end].to_owned()
}

fn digest_answer(user: &str, pass: &str, nonce: &str, uri: &str, with_user: bool) -> String {
    let ha1 = md5_hex(&format!("{user}:{REALM}:{pass}"));
    let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));
    let response = md5_hex(&format!("{ha1}:{nonce}:00000001:cn:auth:{ha2}"));
    let username = if with_user {
        format!("username=\"{user}\", ")
    } else {
        String::new()
    };
    format!(
        "Digest {username}realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", qop=auth, nc=00000001, cnonce=\"cn\""
    )
}

/// Builds the `Authorization` value from the nonce of the first challenge.
type Answer<'a> = Box<dyn Fn(&str) -> String + 'a>;

#[test]
fn malformed_authorization_headers_are_challenged_again() {
    let basic = server(Source::Builder, AuthScheme::Basic, "admin", "secret");
    let basic_rows: Vec<(&str, Option<String>)> = vec![
        ("no header", None),
        ("base64 of `admin` (no colon)", Some(format!("Basic {}", b64(b"admin")))),
        ("base64 of `:secret` (no user)", Some(format!("Basic {}", b64(b":secret")))),
        ("base64 of `:`", Some(format!("Basic {}", b64(b":")))),
        ("base64 of `admin:` (wrong password)", Some(format!("Basic {}", b64(b"admin:")))),
        ("base64 of `adm:secret` (a prefix of the user)", Some(format!("Basic {}", b64(b"adm:secret")))),
        ("base64 of `admin2:secret` (the user plus more)", Some(format!("Basic {}", b64(b"admin2:secret")))),
        ("not base64", Some("Basic not-base64!!".into())),
        (
            "base64 of invalid UTF-8",
            Some(format!("Basic {}", b64(&[0xff, 0xfe, b':', b'x']))),
        ),
        ("nothing after the scheme", Some("Basic ".into())),
        ("an empty value", Some(String::new())),
        (
            "a Digest answer to a Basic server",
            Some("Digest username=\"admin\", realm=\"table\", nonce=\"x\", uri=\"x\", response=\"x\"".into()),
        ),
        ("a Bearer token", Some("Bearer abc".into())),
    ];
    for (label, authorization) in &basic_rows {
        let response = answer_to(&basic, authorization.as_deref());
        assert_challenged(&response, "Basic", label);
    }
    // Control: the right pair is admitted, so the refusals above are about
    // the headers.
    let right = format!("Basic {}", b64(b"admin:secret"));
    assert!(status(&answer_to(&basic, Some(&right))).contains("200"));
    basic.stop().ok();

    let digest = server(Source::Builder, AuthScheme::DigestMd5, "admin", "secret");
    let port = digest.local_addr().expect("local_addr").port();
    let uri = format!("rtsp://127.0.0.1:{port}/live");
    // Each row: a fresh connection, the first 401 supplies the nonce, then
    // the answer under test.
    let digest_rows: Vec<(&str, Answer<'_>)> = vec![
        (
            "the right answer (control)",
            Box::new(|n| digest_answer("admin", "secret", n, &uri, true)),
        ),
        (
            "username=\"\"",
            Box::new(|n| digest_answer("", "secret", n, &uri, true)),
        ),
        (
            "no username parameter",
            Box::new(|n| digest_answer("", "secret", n, &uri, false)),
        ),
        (
            "a prefix of the user",
            Box::new(|n| digest_answer("adm", "secret", n, &uri, true)),
        ),
        (
            "the user plus more",
            Box::new(|n| digest_answer("admin2", "secret", n, &uri, true)),
        ),
        (
            "a Basic answer to a Digest server",
            Box::new(|_| format!("Basic {}", b64(b"admin:secret"))),
        ),
        ("a Bearer token", Box::new(|_| "Bearer abc".to_owned())),
        (
            "garbage after the scheme",
            Box::new(|_| "Digest ,,,===\"\"".to_owned()),
        ),
        (
            "nothing after the scheme",
            Box::new(|_| "Digest ".to_owned()),
        ),
        ("an empty value", Box::new(|_| String::new())),
    ];
    for (label, build) in &digest_rows {
        let mut tcp = connect(&digest);
        let first = try_request(&mut tcp, &describe_request(port, 1, None)).expect("first answer");
        assert_challenged(&first, "Digest", "the unauthenticated request");
        let nonce = param(header(&first, "WWW-Authenticate"), "nonce");
        let answer = build(&nonce);
        let response = try_request(&mut tcp, &describe_request(port, 2, Some(&answer)))
            .unwrap_or_else(|| panic!("{label}: the connection closed instead of answering"));
        if label.starts_with("the right answer") {
            assert!(
                status(&response).contains("200"),
                "{label}: {:?}",
                status(&response)
            );
        } else {
            assert_challenged(&response, "Digest", label);
        }
    }
    digest.stop().ok();
}

/// Three refused requests on one connection end it; the third is still
/// answered.
#[test]
fn the_fourth_unauthenticated_request_finds_the_connection_closed() {
    for scheme in [AuthScheme::Basic, AuthScheme::DigestMd5] {
        let server = server(Source::Builder, scheme, "admin", "secret");
        let port = server.local_addr().expect("local_addr").port();
        let mut tcp = connect(&server);
        let mut answers = Vec::new();
        for cseq in 1..=5 {
            answers.push(try_request(&mut tcp, &describe_request(port, cseq, None)));
        }
        let statuses: Vec<Option<&str>> =
            answers.iter().map(|a| a.as_deref().map(status)).collect();
        for (i, s) in statuses.iter().take(3).enumerate() {
            assert!(
                s.is_some_and(|s| s.contains("401")),
                "{scheme:?}: request {} read {s:?}",
                i + 1
            );
        }
        assert_eq!(
            &statuses[3..],
            &[None, None],
            "{scheme:?}: after the third 401"
        );
        server.stop().ok();
    }
}
