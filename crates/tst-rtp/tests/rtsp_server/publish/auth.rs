//! The publisher role is auth-gated: ANNOUNCE, SETUP `mode=record` and RECORD
//! answer 401 (with a challenge) without credentials and 200 with them, under
//! Basic and under Digest-MD5 (which hashes the method token).

use std::net::TcpStream;
use std::time::Duration;

use base64::Engine as _;
use md5::{Digest as _, Md5};
use secrecy::SecretString;
use tst_rtp::RtspServerBuilder;

use crate::fixtures::raw_rtsp::request;
use crate::fixtures::raw_rtsp_publisher::SDP_MP2T;

fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    resp.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}
fn status(resp: &str) -> u16 {
    resp.split_whitespace().nth(1).unwrap().parse().unwrap()
}
fn connect(port: u16) -> TcpStream {
    let t = TcpStream::connect(("127.0.0.1", port)).unwrap();
    t.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
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

#[test]
fn basic_auth_gates_announce_setup_record_and_admits_credentials() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_basic("test-realm", "admin", SecretString::new("secret".into()));
    let server = b.build().unwrap();
    let mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/pub");
    let creds = format!(
        "Authorization: Basic {}\r\n",
        base64::engine::general_purpose::STANDARD.encode("admin:secret")
    );

    let mut tcp = connect(port);
    // Without credentials: 401 + a challenge, and the slot stays free.
    let r = announce(&mut tcp, &uri, 1, "");
    assert_eq!(status(&r), 401, "{r}");
    assert!(
        header(&r, "WWW-Authenticate").is_some_and(|h| h.starts_with("Basic")),
        "{r}"
    );
    assert!(
        mount.publisher().is_none(),
        "a refused ANNOUNCE must not claim the slot"
    );
    // With credentials: 200 and the slot is held.
    let r = announce(&mut tcp, &uri, 2, &creds);
    assert_eq!(status(&r), 200, "{r}");
    assert!(mount.publisher().is_some());
    // SETUP mode=record and RECORD are gated too.
    let r = request(
        &mut tcp,
        &format!(
            "SETUP {uri}/streamid=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 401, "{r}");
    let r = request(
        &mut tcp,
        &format!(
            "SETUP {uri}/streamid=0 RTSP/1.0\r\nCSeq: 4\r\n{creds}Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n"
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    let sid = header(&r, "Session")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let r = request(
        &mut tcp,
        &format!("RECORD {uri} RTSP/1.0\r\nCSeq: 5\r\nSession: {sid}\r\n\r\n"),
    );
    assert_eq!(status(&r), 401, "{r}");
    let r = request(
        &mut tcp,
        &format!("RECORD {uri} RTSP/1.0\r\nCSeq: 6\r\nSession: {sid}\r\n{creds}\r\n"),
    );
    assert_eq!(status(&r), 200, "{r}");
    drop(tcp);
    server.stop().ok();
}

/// Digest hashes the METHOD into HA2, so this is the test that pins the
/// `method_token` arms for ANNOUNCE and RECORD.
#[test]
fn digest_md5_auth_hashes_the_announce_and_record_method_tokens() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.auth_digest_md5("test-realm", "admin", SecretString::new("secret".into()));
    let server = b.build().unwrap();
    let _mount = server.add_publish_mount("/pub").unwrap();
    server.start().unwrap();
    let port = server.local_addr().unwrap().port();
    let uri = format!("rtsp://127.0.0.1:{port}/pub");
    let md5 = |s: &str| format!("{:x}", Md5::digest(s.as_bytes()));
    let mut tcp = connect(port);

    let r = announce(&mut tcp, &uri, 1, "");
    assert_eq!(status(&r), 401, "{r}");
    let ch = header(&r, "WWW-Authenticate").expect("challenge");
    assert!(ch.starts_with("Digest"), "{ch}");
    let param = |k: &str| {
        ch.split(',').find_map(|p| {
            let (pk, pv) = p
                .trim()
                .trim_start_matches("Digest ")
                .trim()
                .split_once('=')?;
            (pk.trim() == k).then(|| pv.trim().trim_matches('"').to_owned())
        })
    };
    let (realm, nonce) = (param("realm").unwrap(), param("nonce").unwrap());
    let qop = param("qop").map(|q| q.split(',').next().unwrap().trim().to_owned());
    let ha1 = md5(&format!("admin:{realm}:secret"));
    // The server's nc high-water mark advances on every digest it parses, even
    // one it then refuses, so each request carries a strictly increasing nc.
    let nc = std::cell::Cell::new(0u32);
    let digest_for = |method: &str, uri: &str| {
        nc.set(nc.get() + 1);
        let nc = format!("{:08x}", nc.get());
        let ha2 = md5(&format!("{method}:{uri}"));
        let (resp, tail) = match &qop {
            Some(q) => (
                md5(&format!("{ha1}:{nonce}:{nc}:cafebabe:{q}:{ha2}")),
                format!(r#", qop={q}, nc={nc}, cnonce="cafebabe""#),
            ),
            None => (md5(&format!("{ha1}:{nonce}:{ha2}")), String::new()),
        };
        format!(
            r#"Authorization: Digest username="admin", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{resp}", algorithm=MD5{tail}"#
        ) + "\r\n"
    };
    // A response computed over the WRONG method token is refused …
    let r = announce(&mut tcp, &uri, 2, &digest_for("DESCRIBE", &uri));
    assert_eq!(
        status(&r),
        401,
        "a DESCRIBE-token digest must not open ANNOUNCE: {r}"
    );
    // … and over the right one is admitted (nonce is per connection; nc rises per request).
    let r = announce(&mut tcp, &uri, 3, &digest_for("ANNOUNCE", &uri));
    assert_eq!(status(&r), 200, "{r}");
    let setup_uri = format!("{uri}/streamid=0");
    let r = request(
        &mut tcp,
        &format!(
            "SETUP {setup_uri} RTSP/1.0\r\nCSeq: 4\r\n{}Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\n\r\n",
            digest_for("SETUP", &setup_uri)
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    let sid = header(&r, "Session")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let r = request(
        &mut tcp,
        &format!(
            "RECORD {uri} RTSP/1.0\r\nCSeq: 5\r\nSession: {sid}\r\n{}\r\n",
            digest_for("RECORD", &uri)
        ),
    );
    assert_eq!(status(&r), 200, "{r}");
    drop(tcp);
    server.stop().ok();
}
