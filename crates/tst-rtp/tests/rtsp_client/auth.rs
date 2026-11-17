#![allow(clippy::field_reassign_with_default)]

use crate::fixtures::rtsp_loopback_server::*;

#[test]
fn basic_auth_succeeds() {
    let mut cfg = FixtureConfig::default();
    cfg.auth = AuthMode::Basic;
    let h = FixtureHandle::spawn(cfg);
    let url = format!("rtsp://admin:secret@127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let _opts = client.options().unwrap();
    let _sdp = client.describe().unwrap();
    drop(client);
    drop(h);
}

#[test]
fn digest_md5_auth_succeeds() {
    let mut cfg = FixtureConfig::default();
    cfg.auth = AuthMode::DigestMd5;
    let h = FixtureHandle::spawn(cfg);
    let url = format!("rtsp://admin:secret@127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let _sdp = client.describe().unwrap();
    drop(client);
    drop(h);
}

#[test]
fn digest_sha256_auth_succeeds() {
    let mut cfg = FixtureConfig::default();
    cfg.auth = AuthMode::DigestSha256;
    let h = FixtureHandle::spawn(cfg);
    let url = format!("rtsp://admin:secret@127.0.0.1:{}/test", h.port);
    let mut client = tst_rtp::RtspClient::connect(&url).unwrap();
    let _sdp = client.describe().unwrap();
    drop(client);
    drop(h);
}

// --- Which credentials a URL's userinfo gives a challenge's answer ---
//
// Each row runs against a peer that answers `401` to a DESCRIBE without
// `Authorization`, once challenging Basic and once Digest, and checks the
// header the peer received on the retry.

mod userinfo {
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use base64::Engine as _;
    use md5::{Digest as _, Md5};
    use tst_rtp::{RtspClient, RtspClientBuilder, RtspError, SecretString};
    use tst_test_helpers::timing::scaled;

    use crate::fixtures::raw_rtsp::try_header;
    use crate::fixtures::raw_rtsp_server::{SDP_BODY, cseq_of, method_of};

    #[derive(Clone, Copy, Debug)]
    enum Challenge {
        Basic,
        Digest,
    }

    impl Challenge {
        fn www_authenticate(self) -> &'static str {
            match self {
                Challenge::Basic => r#"Basic realm="x""#,
                Challenge::Digest => r#"Digest realm="x", nonce="n""#,
            }
        }
    }

    /// The next request on `sock`, or `None` once the client has closed it.
    fn next_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
        loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&buf[..pos + 4]).into_owned();
                buf.drain(..pos + 4);
                return Some(req);
            }
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::ConnectionReset => return None,
                Err(e) => panic!("peer read: {e}"),
            }
        }
    }

    /// A peer that answers a DESCRIBE without `Authorization` with `401`
    /// and `challenge`, a DESCRIBE with one with an SDP, and anything else
    /// with `200`. Returns every request it read once the client closes.
    fn challenging_peer(challenge: Challenge) -> (u16, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(scaled(Duration::from_secs(10))))
                .unwrap();
            let mut buf = Vec::new();
            let mut seen = Vec::new();
            while let Some(req) = next_request(&mut sock, &mut buf) {
                let cseq = cseq_of(&req);
                let answer = match (method_of(&req), try_header(&req, "authorization")) {
                    ("DESCRIBE", None) => format!(
                        "RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: {}\r\n\r\n",
                        challenge.www_authenticate()
                    ),
                    ("DESCRIBE", Some(_)) => format!(
                        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
                        SDP_BODY.len(),
                        String::from_utf8_lossy(SDP_BODY)
                    ),
                    _ => format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n"),
                };
                seen.push(req);
                // The client may already have gone (after `AuthFailed`).
                let _ = sock.write_all(answer.as_bytes());
            }
            seen
        });
        (port, peer)
    }

    /// DESCRIBE `rtsp://<userinfo>127.0.0.1:<port>/live` against a peer
    /// challenging with `challenge`, through the builder when `auth` is
    /// set. Returns the call's result and the DESCRIBE requests sent.
    fn describe(
        challenge: Challenge,
        userinfo: &str,
        auth: Option<(&str, &str)>,
    ) -> (Result<(), RtspError>, Vec<String>) {
        let (port, peer) = challenging_peer(challenge);
        let url = format!("rtsp://{userinfo}127.0.0.1:{port}/live");
        let result = (|| {
            let mut client = match auth {
                None => RtspClient::connect(&url)?,
                Some((user, password)) => RtspClientBuilder::new(&url)?
                    .no_auto_keepalive(true)
                    .auth(user, SecretString::new(password.into()))
                    .connect()?,
            };
            client.describe().map(drop)
        })();
        let describes = peer
            .join()
            .unwrap()
            .into_iter()
            .filter(|r| method_of(r) == "DESCRIBE")
            .collect();
        (result, describes)
    }

    fn md5_hex(s: &str) -> String {
        Md5::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The value of `key` in a Digest `Authorization`, quotes removed.
    fn param<'a>(authorization: &'a str, key: &str) -> &'a str {
        let at = authorization
            .find(&format!(" {key}="))
            .unwrap_or_else(|| panic!("no {key} in {authorization}"))
            + key.len()
            + 2;
        authorization[at..]
            .split(',')
            .next()
            .unwrap()
            .trim_matches('"')
    }

    /// The retry answers the challenge with `user` and `password`.
    fn answers_with(
        challenge: Challenge,
        userinfo: &str,
        auth: Option<(&str, &str)>,
        user: &str,
        password: &str,
    ) {
        let (result, describes) = describe(challenge, userinfo, auth);
        result.unwrap_or_else(|e| panic!("{challenge:?} {userinfo:?}: {e:?}"));
        assert_eq!(
            describes.len(),
            2,
            "one DESCRIBE and its retry: {describes:?}"
        );
        assert_eq!(try_header(&describes[0], "authorization"), None);
        let authorization = try_header(&describes[1], "authorization")
            .unwrap_or_else(|| panic!("the retry has no Authorization: {}", describes[1]));
        match challenge {
            Challenge::Basic => {
                let pair =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
                assert_eq!(authorization, format!("Basic {pair}"));
            }
            Challenge::Digest => {
                let uri = describes[1].split_whitespace().nth(1).unwrap();
                assert_eq!(param(authorization, "username"), user);
                assert_eq!(param(authorization, "realm"), "x");
                assert_eq!(param(authorization, "nonce"), "n");
                assert_eq!(param(authorization, "uri"), uri);
                let ha1 = md5_hex(&format!("{user}:x:{password}"));
                let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));
                assert_eq!(
                    param(authorization, "response"),
                    md5_hex(&format!("{ha1}:n:{ha2}"))
                );
            }
        }
    }

    /// No request carries `Authorization` and the call ends in `AuthFailed`.
    fn never_answers(challenge: Challenge, userinfo: &str) {
        let (result, describes) = describe(challenge, userinfo, None);
        assert!(
            matches!(result, Err(RtspError::AuthFailed)),
            "{challenge:?} {userinfo:?}: {result:?}"
        );
        assert_eq!(describes.len(), 1, "no retry: {describes:?}");
        assert_eq!(try_header(&describes[0], "authorization"), None);
    }

    #[test]
    fn basic_user_and_password() {
        answers_with(Challenge::Basic, "u:p@", None, "u", "p");
    }

    #[test]
    fn digest_user_and_password() {
        answers_with(Challenge::Digest, "u:p@", None, "u", "p");
    }

    #[test]
    fn basic_user_and_empty_password() {
        answers_with(Challenge::Basic, "u:@", None, "u", "");
    }

    #[test]
    fn digest_user_and_empty_password() {
        answers_with(Challenge::Digest, "u:@", None, "u", "");
    }

    #[test]
    fn basic_user_without_password_answers_with_an_empty_one() {
        answers_with(Challenge::Basic, "u@", None, "u", "");
    }

    #[test]
    fn digest_user_without_password_answers_with_an_empty_one() {
        answers_with(Challenge::Digest, "u@", None, "u", "");
    }

    #[test]
    fn basic_no_userinfo_never_answers() {
        never_answers(Challenge::Basic, "");
    }

    #[test]
    fn digest_no_userinfo_never_answers() {
        never_answers(Challenge::Digest, "");
    }

    #[test]
    fn basic_percent_encoded_pair_is_decoded() {
        answers_with(Challenge::Basic, "u%40x:p%3As@", None, "u@x", "p:s");
    }

    #[test]
    fn digest_percent_encoded_pair_is_decoded() {
        answers_with(Challenge::Digest, "u%40x:p%3As@", None, "u@x", "p:s");
    }

    #[test]
    fn basic_builder_auth_overrides_the_url() {
        answers_with(Challenge::Basic, "u:p@", Some(("b", "q")), "b", "q");
    }

    #[test]
    fn digest_builder_auth_overrides_the_url() {
        answers_with(Challenge::Digest, "u:p@", Some(("b", "q")), "b", "q");
    }

    // `RtspUrl::parse` accepts both and records an empty user name.

    #[test]
    fn basic_empty_user_with_password_never_answers() {
        never_answers(Challenge::Basic, ":p@");
    }

    #[test]
    fn digest_empty_user_with_password_never_answers() {
        never_answers(Challenge::Digest, ":p@");
    }

    #[test]
    fn basic_empty_userinfo_never_answers() {
        never_answers(Challenge::Basic, "@");
    }

    #[test]
    fn digest_empty_userinfo_never_answers() {
        never_answers(Challenge::Digest, "@");
    }
}
