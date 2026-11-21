use super::*;

const URI: &str = "rtsp://cam.example/live";

fn cfg(creds: Option<(&str, &str)>) -> EngineConfig {
    EngineConfig {
        uri: URI.to_string(),
        version: RtspVersion::V1_0,
        user_agent: "tst-rtp/test".to_string(),
        credentials: creds.map(|(u, p)| Credentials {
            username: u.to_string(),
            password: SecretString::new(p.into()),
        }),
        schemes: AuthSchemes::Automatic,
    }
}

fn spec(method: RtspMethod, uri: &str) -> RequestSpec {
    RequestSpec {
        method,
        uri: uri.to_string(),
        headers: Vec::new(),
        body: None,
        announce_tracks: None,
        setup: None,
    }
}

/// `method` at `URI` carrying `body` as `application/sdp`; an ANNOUNCE
/// announces one track.
fn with_sdp(method: RtspMethod, body: &[u8]) -> RequestSpec {
    RequestSpec {
        body: Some(RequestBody {
            content_type: "application/sdp",
            bytes: Bytes::copy_from_slice(body),
        }),
        announce_tracks: (method == RtspMethod::Announce).then_some(1),
        ..spec(method, URI)
    }
}

fn resp(bytes: &[u8]) -> RtspResponse {
    RtspResponse::parse(bytes).unwrap().0
}

fn ms(t0: Instant, n: u64) -> Instant {
    t0 + Duration::from_millis(n)
}

fn sent(t: &Transmit) -> RtspRequest {
    RtspRequest::parse(&t.bytes).unwrap().0
}

fn cseq(t: &Transmit) -> u32 {
    sent(t).headers["cseq"].parse().unwrap()
}

fn authz(t: &Transmit) -> Option<String> {
    sent(t).headers.get("authorization").cloned()
}

fn param<'a>(authorization: &'a str, key: &str) -> &'a str {
    let at = authorization
        .find(&format!("{key}="))
        .unwrap_or_else(|| panic!("no {key} in {authorization}"))
        + key.len()
        + 1;
    authorization[at..]
        .split(',')
        .next()
        .unwrap()
        .trim_matches('"')
}

fn transmits(e: &mut ClientEngine) -> Vec<Transmit> {
    std::iter::from_fn(|| e.poll_transmit()).collect()
}

fn events(e: &mut ClientEngine) -> Vec<Event> {
    std::iter::from_fn(|| e.poll_event()).collect()
}

fn one(e: &mut ClientEngine) -> Transmit {
    let mut t = transmits(e);
    assert_eq!(t.len(), 1, "expected one transmit, got {t:?}");
    t.remove(0)
}

/// The only event is the reply to `txn`; returns its result.
fn outcome(e: &mut ClientEngine, txn: TxnId) -> Result<ReplyOutcome, RtspError> {
    let mut ev = events(e);
    assert_eq!(ev.len(), 1, "expected one event, got {ev:?}");
    match ev.remove(0) {
        Event::Reply { txn: t, result } => {
            assert_eq!(t, txn);
            result
        }
        other => panic!("expected a reply, got {other:?}"),
    }
}

/// [`outcome`], the response alone.
fn reply(e: &mut ClientEngine, txn: TxnId) -> Result<RtspResponse, RtspError> {
    outcome(e, txn).map(|o| o.response)
}

const BASIC_401: &[u8] =
    b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n";

fn md5_hex(s: &str) -> String {
    use md5::{Digest, Md5};
    Md5::digest(s.as_bytes())
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// An engine whose cached challenge is `www`, learned from a `401` to an
/// OPTIONS; the retry and its `200` are consumed.
fn challenged(creds: Option<(&str, &str)>, www: &str) -> ClientEngine {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(creds));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let first = one(&mut e);
    let c = cseq(&first);
    e.handle_reply(
        resp(
            format!("RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: {www}\r\n\r\n")
                .as_bytes(),
        ),
        t0,
    );
    feed(&mut e);
    if creds.is_some() {
        let retry = one(&mut e);
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&retry)).as_bytes()),
            t0,
        );
        assert_eq!(reply(&mut e, a).unwrap().status, 200);
    } else {
        assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
    }
    e
}

// --- submit ---

#[test]
fn submit_writes_cseq_user_agent_and_the_spec_headers() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let mut describe = spec(RtspMethod::Describe, URI);
    describe
        .headers
        .push(("accept", "application/sdp".to_string()));
    let a = e.submit(describe, None, t0).unwrap();
    let t = one(&mut e);
    assert_eq!(t.txn, Some(a));
    assert!(!t.is_ping);
    assert!(
        t.bytes
            .starts_with(b"DESCRIBE rtsp://cam.example/live RTSP/1.0\r\n"),
        "{:?}",
        String::from_utf8_lossy(&t.bytes)
    );
    let r = sent(&t);
    let mut names: Vec<_> = r.headers.keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["accept", "cseq", "user-agent"]);
    assert_eq!(r.headers["cseq"], "1");
    assert_eq!(r.headers["user-agent"], "tst-rtp/test");
    assert_eq!(r.headers["accept"], "application/sdp");

    let b = e
        .submit(spec(RtspMethod::Play, "rtsp://cam.example/live/"), None, t0)
        .unwrap();
    assert_ne!(a, b);
    let t = one(&mut e);
    assert!(
        t.bytes
            .starts_with(b"PLAY rtsp://cam.example/live/ RTSP/1.0\r\n")
    );
    assert_eq!(cseq(&t), 2);
    assert!(events(&mut e).is_empty());
}

#[test]
fn submit_uses_the_configured_version() {
    let mut c = cfg(None);
    c.version = RtspVersion::V2_0;
    let mut e = ClientEngine::new(c);
    e.submit(spec(RtspMethod::Options, URI), None, Instant::now())
        .unwrap();
    assert!(
        one(&mut e)
            .bytes
            .starts_with(b"OPTIONS rtsp://cam.example/live RTSP/2.0\r\n")
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a header the engine writes itself")]
fn a_spec_carrying_an_engine_header_is_a_bug() {
    let mut bad = spec(RtspMethod::Describe, URI);
    bad.headers.push(("CSeq", "9".to_string()));
    let _ = ClientEngine::new(cfg(None)).submit(bad, None, Instant::now());
}

#[test]
fn debug_shows_no_request_bytes_and_no_credentials() {
    let mut e = challenged(Some(("user", "s3cret-pw")), r#"Basic realm="cam""#);
    e.submit(spec(RtspMethod::Describe, URI), None, Instant::now())
        .unwrap();
    // The queued request is authorised.
    let basic = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode("user:s3cret-pw")
    };
    let t = e.poll_transmit().unwrap();
    assert!(
        String::from_utf8_lossy(&t.bytes).contains(&basic),
        "the fixture must hold the Basic header"
    );
    e.transmits.push_front(t);
    // A derived `Debug` would print the bytes as a list of numbers.
    let as_numbers = format!("{:?}", basic.as_bytes());
    let as_numbers = as_numbers.trim_matches(['[', ']']);
    for text in [
        format!("{e:?}"),
        format!("{e:#?}"),
        format!("{:?}", e.transmits),
    ] {
        assert!(!text.contains("s3cret-pw"), "{text}");
        assert!(!text.contains(&basic), "{text}");
        assert!(!text.contains(as_numbers), "{text}");
    }
}

#[test]
fn an_injected_header_is_refused_and_nothing_is_queued() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let mut bad = spec(RtspMethod::Describe, URI);
    bad.headers
        .push(("x-custom", "a\r\nInjected: 1".to_string()));
    assert!(matches!(
        e.submit(bad, None, t0),
        Err(RtspError::InvalidHeader { .. })
    ));
    assert!(e.poll_transmit().is_none());
    assert!(e.poll_event().is_none());
    assert!(matches!(
        e.submit(
            spec(RtspMethod::Setup, "rtsp://cam.example/live\r\nX: y"),
            None,
            t0
        ),
        Err(RtspError::InvalidHeader { .. })
    ));
    assert!(e.poll_transmit().is_none());
}

/// Some servers end the connection unless each request's CSeq is one above
/// the last, so a request that never reaches the wire takes no CSeq.
#[test]
fn a_request_refused_at_encoding_uses_no_cseq() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    assert_eq!(cseq(&one(&mut e)), 1);
    assert!(matches!(
        e.submit(
            spec(RtspMethod::Setup, "rtsp://cam.example/live\x01"),
            None,
            t0
        ),
        Err(RtspError::InvalidHeader { .. })
    ));
    assert!(e.poll_transmit().is_none());
    e.submit(spec(RtspMethod::Setup, URI), None, t0).unwrap();
    assert_eq!(cseq(&one(&mut e)), 2);
}

/// The `401` retry of a request that cannot be encoded takes no CSeq
/// either: the next request carries the one the retry would have had.
#[test]
fn a_retry_refused_at_encoding_uses_no_cseq() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    assert_eq!(cseq(&one(&mut e)), 1);
    // A challenge whose realm carries a control byte cannot be echoed in
    // the retry's `Authorization` header.
    e.handle_reply(
        resp(b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Digest realm=\"R\x01\", nonce=\"n\"\r\n\r\n"),
        t0,
    );
    feed(&mut e);
    assert!(matches!(
        reply(&mut e, a),
        Err(RtspError::InvalidHeader { .. })
    ));
    assert!(e.poll_transmit().is_none());
    // The cached challenge would refuse every later request too.
    e.auth.challenge = None;
    e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    assert_eq!(cseq(&one(&mut e)), 2);
}

#[test]
fn a_new_submit_forgets_an_unanswered_transaction() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let b = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    assert_eq!(transmits(&mut e).len(), 2);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
    assert!(events(&mut e).is_empty(), "A was forgotten");
    assert_eq!(e.counters().unmatched_replies, 1);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n"), t0);
    assert_eq!(reply(&mut e, b).unwrap().status, 200);
    assert_ne!(a, b);
}

// --- authentication ---

/// Port of `options_authenticates_on_challenge`.
#[test]
fn a_401_is_retried_once_with_credentials_under_the_same_txn() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let first = one(&mut e);
    assert_eq!(authz(&first), None);
    e.handle_reply(resp(BASIC_401), t0);
    assert!(events(&mut e).is_empty());
    let retry = one(&mut e);
    assert_eq!(retry.txn, None, "the retry reports no second write");
    assert!(!retry.is_ping);
    assert_eq!(cseq(&retry), 2);
    assert!(
        authz(&retry)
            .unwrap()
            .to_ascii_lowercase()
            .starts_with("basic "),
        "{retry:?}"
    );
    e.handle_reply(
        resp(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY\r\n\r\n"),
        t0,
    );
    let ok = reply(&mut e, a).unwrap();
    assert!(ok.headers["public"].contains("DESCRIBE"));
}

#[test]
fn a_401_without_www_authenticate_is_bad_response() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\n\r\n"), t0);
    assert!(matches!(
        reply(&mut e, a),
        Err(RtspError::BadResponse {
            detail: "401 without WWW-Authenticate header"
        })
    ));
    assert!(e.poll_transmit().is_none());
}

#[test]
fn an_empty_password_answers_basic_with_the_user_and_a_colon() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("u", ""))));
    e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), t0);
    let expected = {
        use base64::Engine as _;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u:")
        )
    };
    assert_eq!(authz(&one(&mut e)), Some(expected));
    assert!(events(&mut e).is_empty());
}

#[test]
fn a_401_without_credentials_is_auth_failed() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), t0);
    assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
    assert!(e.poll_transmit().is_none());
}

#[test]
fn a_second_401_is_auth_failed() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), t0);
    let retry = one(&mut e);
    e.handle_reply(
        resp(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n",
                cseq(&retry)
            )
            .as_bytes(),
        ),
        t0,
    );
    assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
    assert!(e.poll_transmit().is_none(), "no third attempt");
}

#[test]
fn a_scheme_refused_by_the_policy_is_auth_unsupported() {
    let t0 = Instant::now();
    let mut c = cfg(Some(("user", "pw")));
    c.schemes = AuthSchemes::DigestOnly;
    let mut e = ClientEngine::new(c);
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), t0);
    assert!(matches!(
        reply(&mut e, a),
        Err(RtspError::AuthUnsupported { .. })
    ));
    assert!(e.poll_transmit().is_none());
    // The challenge stays cached, so the next request's pre-emptive
    // header fails the same way and nothing is queued.
    assert!(matches!(
        e.submit(spec(RtspMethod::Describe, URI), None, t0),
        Err(RtspError::AuthUnsupported { .. })
    ));
    assert!(e.poll_transmit().is_none());
}

#[test]
fn teardown_is_retried_after_a_401_too() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    let mut td = spec(RtspMethod::Teardown, URI);
    td.headers.push(("session", "abc".to_string()));
    let a = e.submit(td, None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), t0);
    let retry = one(&mut e);
    let r = sent(&retry);
    assert_eq!(r.method, RtspMethod::Teardown);
    assert_eq!(r.headers["session"], "abc");
    assert!(r.headers.contains_key("authorization"));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&retry)).as_bytes()),
        t0,
    );
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::Ended(EndReason::CleanTeardown), Event::Reply { txn, result: Ok(_) }] if *txn == a
        ),
        "{ev:?}"
    );
}

#[test]
fn a_teardown_whose_preemptive_header_fails_goes_out_without_one() {
    let t0 = Instant::now();
    let mut c = cfg(Some(("user", "pw")));
    c.schemes = AuthSchemes::DigestOnly;
    let mut e = ClientEngine::new(c);
    e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), t0);
    events(&mut e);
    e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    assert_eq!(authz(&one(&mut e)), None);
}

/// Port of `preemptive_authorization_builds_setup_digest_for_control_uri`.
#[test]
fn a_cached_challenge_signs_setup_against_its_control_uri() {
    let mut e = challenged(
        Some(("user", "pass")),
        r#"Digest realm="tstrans", nonce="abc123""#,
    );
    let control = "rtsp://cam.example/live/trackID=0";
    feed(&mut e);
    e.submit(spec(RtspMethod::Setup, control), None, Instant::now())
        .unwrap();
    let t = one(&mut e);
    let hdr = authz(&t).expect("cached challenge + credentials → pre-emptive header");
    let ha1 = md5_hex("user:tstrans:pass");
    let ha2 = md5_hex(&format!("SETUP:{control}"));
    let expected = md5_hex(&format!("{ha1}:abc123:{ha2}"));
    assert!(hdr.contains(&format!("response=\"{expected}\"")), "{hdr}");
    assert!(hdr.contains(&format!("uri=\"{control}\"")), "{hdr}");
    assert!(hdr.contains(r#"username="user""#), "{hdr}");
}

/// Port of `preemptive_authorization_inert_without_challenge_or_creds`.
#[test]
fn no_preemptive_header_without_a_challenge_or_credentials() {
    let t0 = Instant::now();
    // Credentials, no challenge yet.
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
    assert_eq!(authz(&one(&mut e)), None);
    // A challenge, no credentials.
    let mut e = challenged(None, r#"Digest realm="R", nonce="n""#);
    e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
    assert_eq!(authz(&one(&mut e)), None);
}

/// Port of `qop_auth_nonce_count_increments_and_resets_on_new_challenge`.
#[test]
fn the_nonce_count_counts_per_header_and_restarts_on_a_new_challenge() {
    let t0 = Instant::now();
    let www1 = r#"Digest realm="R", nonce="n1", qop="auth""#;
    let mut e = challenged(Some(("user", "pass")), www1);
    // The challenge's own retry used nc=1.
    let mut ncs = Vec::new();
    for m in [RtspMethod::Describe, RtspMethod::Setup, RtspMethod::Play] {
        feed(&mut e);
        e.submit(spec(m, URI), None, t0).unwrap();
        let a = authz(&one(&mut e)).unwrap();
        assert!(a.contains("qop=auth"), "{a}");
        assert_eq!(param(&a, "nonce"), "n1");
        ncs.push(param(&a, "nc").to_string());
    }
    assert_eq!(ncs, ["00000002", "00000003", "00000004"]);

    // A different challenge (new nonce) restarts the count: its retry is nc=1.
    feed(&mut e);
    e.submit(spec(RtspMethod::Setup, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n2\", qop=\"auth\"\r\n\r\n"
            )
            .as_bytes(),
        ),
        t0,
    );
    feed(&mut e);
    let retry = one(&mut e);
    let a = authz(&retry).unwrap();
    assert_eq!((param(&a, "nonce"), param(&a, "nc")), ("n2", "00000001"));

    // The same challenge again does not restart it.
    e.handle_reply(
        resp(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n2\", qop=\"auth\"\r\n\r\n",
                cseq(&retry)
            )
            .as_bytes(),
        ),
        t0,
    );
    events(&mut e);
    feed(&mut e);
    e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
    let a = authz(&one(&mut e)).unwrap();
    assert_eq!((param(&a, "nonce"), param(&a, "nc")), ("n2", "00000002"));
}

// --- the Digest cnonce ---

/// The bytes a test hands the engine as a cnonce, and their form on the
/// wire.
const CNONCE: Cnonce = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const CNONCE_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// Hand the engine a cnonce: ahead of a request or a ping that answers the
/// cached challenge, and after a `401` whose second attempt waits for it,
/// as the engine's driver does.
fn feed(e: &mut ClientEngine) {
    e.handle_cnonce(Some(CNONCE));
}

const QOP_WWW: &str = r#"Digest realm="R", nonce="n1", qop="auth""#;

fn unauthorized(cseq: u32, www: &str) -> RtspResponse {
    resp(
        format!("RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: {www}\r\n\r\n")
            .as_bytes(),
    )
}

fn ok(cseq: u32) -> RtspResponse {
    resp(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
}

/// RFC 7616 §3.4.1 for MD5 with `qop=auth`, written out:
/// `H(H(user:realm:password):nonce:nc:cnonce:auth:H(method:uri))`.
#[allow(clippy::too_many_arguments)]
fn qop_auth_md5_response(
    user: &str,
    realm: &str,
    password: &str,
    nonce: &str,
    nc: &str,
    cnonce: &str,
    method: &str,
    uri: &str,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
}

#[test]
fn a_fixed_cnonce_gives_the_rfc_7616_response() {
    // The user, password, realm and nonce of RFC 7616 §3.9.1.
    const NONCE: &str = "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v";
    let www = format!(
        r#"Digest realm="http-auth@example.org", qop="auth", algorithm=MD5, nonce="{NONCE}""#
    );
    let expected = |nc: &str, method: &str| {
        qop_auth_md5_response(
            "Mufasa",
            "http-auth@example.org",
            "Circle of Life",
            NONCE,
            nc,
            CNONCE_HEX,
            method,
            URI,
        )
    };
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("Mufasa", "Circle of Life"))));
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(unauthorized(c, &www), t0);
    // The answer waits for its cnonce: nothing is written, nothing reported.
    assert!(e.wants_cnonce());
    assert!(e.poll_transmit().is_none());
    assert!(e.poll_event().is_none());
    e.handle_cnonce(Some(CNONCE));
    assert!(!e.wants_cnonce());
    let retry = one(&mut e);
    let h = authz(&retry).unwrap();
    assert_eq!(param(&h, "cnonce"), CNONCE_HEX);
    assert_eq!(param(&h, "nc"), "00000001");
    assert_eq!(param(&h, "response"), expected("00000001", "DESCRIBE"));
    e.handle_reply(ok(cseq(&retry)), t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);

    // The cached challenge's answer on the next request.
    let next = spec(RtspMethod::Setup, URI);
    assert!(e.submit_wants_cnonce(&next));
    e.handle_cnonce(Some(CNONCE));
    assert!(!e.submit_wants_cnonce(&next), "one is at hand");
    e.submit(next, None, t0).unwrap();
    let h = authz(&one(&mut e)).unwrap();
    assert_eq!(param(&h, "cnonce"), CNONCE_HEX);
    assert_eq!(param(&h, "response"), expected("00000002", "SETUP"));

    // And on a ping.
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    assert!(
        !e.timeout_wants_cnonce(ms(t0, 999), false),
        "no ping is due"
    );
    assert!(e.timeout_wants_cnonce(ms(t0, 1000), false));
    e.handle_cnonce(Some(CNONCE));
    e.handle_timeout(ms(t0, 1000), false);
    let pings: Vec<_> = transmits(&mut e)
        .into_iter()
        .filter(|t| t.is_ping)
        .collect();
    assert_eq!(pings.len(), 1);
    let h = authz(&pings[0]).unwrap();
    assert_eq!(param(&h, "cnonce"), CNONCE_HEX);
    assert_eq!(param(&h, "response"), expected("00000003", "OPTIONS"));
}

#[test]
fn a_random_source_failure_fails_the_request_as_before() {
    let t0 = Instant::now();
    // The retry after a `401`: the request fails, nothing is written.
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(unauthorized(c, QOP_WWW), t0);
    assert!(e.wants_cnonce());
    e.handle_cnonce(None);
    assert!(!e.wants_cnonce());
    assert!(matches!(
        reply(&mut e, a),
        Err(RtspError::Io(io::ErrorKind::Other))
    ));
    assert!(e.poll_transmit().is_none());

    // The cached challenge's answer: the request is refused and takes no CSeq.
    assert!(matches!(
        e.submit(spec(RtspMethod::Setup, URI), None, t0),
        Err(RtspError::Io(io::ErrorKind::Other))
    ));
    assert!(e.poll_transmit().is_none());
    // A ping goes out without a header.
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    let ping = one(&mut e);
    assert!(ping.is_ping);
    assert_eq!(authz(&ping), None);
    assert_eq!(cseq(&ping), c + 1);
    // So does a TEARDOWN.
    e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    assert_eq!(authz(&one(&mut e)), None);
}

/// The driver draws a cnonce only when the engine asks, so the engine must
/// ask exactly when its next step builds a Digest answer.
#[test]
fn a_cnonce_is_asked_for_only_when_a_digest_answer_is_built() {
    let t0 = Instant::now();
    let options = || spec(RtspMethod::Options, URI);

    // No challenge yet, with or without credentials.
    for creds in [None, Some(("user", "pass"))] {
        let mut e = ClientEngine::new(cfg(creds));
        e.enable_keepalive(Some(Duration::from_secs(1)), t0);
        assert!(!e.submit_wants_cnonce(&options()));
        assert!(!e.timeout_wants_cnonce(ms(t0, 1000), false));
        assert!(!e.wants_cnonce());
    }

    // A Digest challenge and no credentials: the request fails at once.
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(options(), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(unauthorized(c, QOP_WWW), t0);
    assert!(!e.wants_cnonce());
    assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
    assert!(!e.submit_wants_cnonce(&options()));

    // A Basic challenge is answered without one, at once and later.
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    e.submit(options(), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(unauthorized(c, r#"Basic realm="cam""#), t0);
    assert!(!e.wants_cnonce());
    assert!(authz(&one(&mut e)).is_some_and(|a| a.starts_with("Basic ")));
    assert!(!e.submit_wants_cnonce(&options()));

    // A Digest challenge with credentials.
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    let a = e.submit(options(), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(unauthorized(c, QOP_WWW), t0);
    assert!(e.wants_cnonce());
    e.handle_cnonce(Some(CNONCE));
    let retry = one(&mut e);
    // A second `401` is the end of the request, not another answer.
    e.handle_reply(unauthorized(cseq(&retry), QOP_WWW), t0);
    assert!(!e.wants_cnonce());
    assert!(matches!(reply(&mut e, a), Err(RtspError::AuthFailed)));
    // A request the engine refuses builds no answer.
    assert!(e.submit_wants_cnonce(&options()));
    assert!(!e.submit_wants_cnonce(&spec(RtspMethod::Record, URI)));
    // Nor does a ping that is not due, or a cancelled client's.
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    assert!(!e.timeout_wants_cnonce(ms(t0, 999), false));
    assert!(!e.timeout_wants_cnonce(ms(t0, 1000), true));
    assert!(e.timeout_wants_cnonce(ms(t0, 1000), false));
    // A `401` to a ping only refreshes the challenge.
    e.handle_cnonce(Some(CNONCE));
    e.handle_timeout(ms(t0, 1000), false);
    let ping = one(&mut e);
    e.handle_reply(
        unauthorized(cseq(&ping), r#"Digest realm="R", nonce="n2", qop="auth""#),
        ms(t0, 1000),
    );
    assert!(!e.wants_cnonce());
    assert!(e.poll_transmit().is_none());
    // Nothing is asked for once the stream has ended.
    e.handle_closed(StreamEnd::PeerClosed);
    assert!(!e.submit_wants_cnonce(&options()));
    assert!(!e.timeout_wants_cnonce(ms(t0, 2000), false));
}

#[test]
fn a_cnonce_handed_in_before_the_401_is_used_without_asking() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_cnonce(Some(CNONCE));
    e.handle_reply(unauthorized(c, QOP_WWW), t0);
    assert!(!e.wants_cnonce());
    let h = authz(&one(&mut e)).unwrap();
    assert_eq!(param(&h, "cnonce"), CNONCE_HEX);
}

/// A nonce's count is taken where its `401` is read, although the answer
/// is built later: a ping's `401` behind it in the same read, naming a
/// newer nonce, leaves that nonce its first count for the next request.
#[test]
fn a_ping_401_behind_the_requests_401_leaves_the_newer_nonce_its_first_count() {
    let www = |nonce: &str| format!(r#"Digest realm="R", nonce="{nonce}", qop="auth""#);
    let t0 = Instant::now();
    let mut e = challenged(Some(("user", "pass")), &www("n1"));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    feed(&mut e);
    let a = e
        .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
        .unwrap();
    let request = one(&mut e);
    feed(&mut e);
    e.handle_timeout(ms(t0, 1000), false);
    let ping = one(&mut e);
    // One read: the request's `401`, then the ping's. No cnonce is at hand.
    e.handle_reply(unauthorized(cseq(&request), &www("n2")), ms(t0, 1010));
    e.handle_reply(unauthorized(cseq(&ping), &www("n3")), ms(t0, 1010));
    assert!(e.wants_cnonce());
    assert!(e.poll_transmit().is_none());
    feed(&mut e);
    let retry = one(&mut e);
    let h = authz(&retry).unwrap();
    assert_eq!((param(&h, "nonce"), param(&h, "nc")), ("n2", "00000001"));
    e.handle_reply(ok(cseq(&retry)), ms(t0, 1020));
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    // The first request under the ping's nonce.
    feed(&mut e);
    e.submit(spec(RtspMethod::Play, URI), None, ms(t0, 1030))
        .unwrap();
    let h = authz(&one(&mut e)).unwrap();
    assert_eq!((param(&h, "nonce"), param(&h, "nc")), ("n3", "00000001"));
}

/// While its answer waits for a cnonce the request is still the caller's:
/// it times out, ends with the stream and is replaced like any other, and
/// no reply is taken for it.
#[test]
fn a_request_waiting_for_its_cnonce_is_still_the_callers() {
    let t0 = Instant::now();
    let waiting = |deadline: Option<Instant>| {
        let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
        let a = e
            .submit(spec(RtspMethod::Options, URI), deadline, t0)
            .unwrap();
        let c = cseq(&one(&mut e));
        e.handle_reply(unauthorized(c, QOP_WWW), t0);
        assert!(e.wants_cnonce());
        (e, a, c)
    };

    // A reply under the first attempt's CSeq is not its reply, nor is one
    // without a CSeq: no attempt is in flight for either to answer.
    let (mut e, a, c) = waiting(None);
    e.handle_reply(ok(c), t0);
    assert_eq!(e.counters().unmatched_replies, 1);
    assert!(e.poll_event().is_none());
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\n\r\n"), t0);
    assert_eq!(e.counters().unmatched_replies, 2);
    assert!(e.poll_event().is_none());
    assert!(e.wants_cnonce());
    e.handle_cnonce(Some(CNONCE));
    let retry = one(&mut e);
    assert_eq!(cseq(&retry), c + 1);
    e.handle_reply(ok(c + 1), t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);

    // Its deadline still runs.
    let (mut e, a, _) = waiting(Some(ms(t0, 100)));
    e.handle_timeout(ms(t0, 100), false);
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
    assert!(!e.wants_cnonce());
    e.handle_cnonce(Some(CNONCE));
    assert!(e.poll_transmit().is_none(), "nothing left to answer");

    // The stream's end answers it.
    let (mut e, a, _) = waiting(None);
    e.handle_closed(StreamEnd::PeerClosed);
    assert!(!e.wants_cnonce());
    let ev = events(&mut e);
    assert!(
        ev.iter().any(|ev| matches!(
            ev,
            Event::Reply { txn, result: Err(RtspError::Io(io::ErrorKind::UnexpectedEof)) } if *txn == a
        )),
        "{ev:?}"
    );

    // A TEARDOWN takes its place. With no cnonce at hand it goes out
    // without a header, and a cnonce that comes later answers nothing.
    let (mut e, _, c) = waiting(None);
    let b = e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    assert!(!e.wants_cnonce());
    let teardown = one(&mut e);
    assert_eq!(sent(&teardown).method, RtspMethod::Teardown);
    assert_eq!(cseq(&teardown), c + 1);
    e.handle_cnonce(Some(CNONCE));
    assert!(e.poll_transmit().is_none(), "the replaced request is gone");
    e.handle_reply(ok(c + 1), t0);
    let ev = events(&mut e);
    assert!(
        ev.iter()
            .any(|ev| matches!(ev, Event::Reply { txn, result: Ok(_) } if *txn == b)),
        "{ev:?}"
    );
}

// --- session and keepalive timing ---

fn setup_200(cseq: u32, session: &str) -> RtspResponse {
    resp(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nSession: {session}\r\n\r\n"
        )
        .as_bytes(),
    )
}

/// Runs a SETUP answered with `Session: <session>` at `at`.
fn run_setup(e: &mut ClientEngine, session: &str, at: Instant) {
    let a = e.submit(spec(RtspMethod::Setup, URI), None, at).unwrap();
    let c = cseq(&one(e));
    e.handle_reply(setup_200(c, session), at);
    assert_eq!(reply(e, a).unwrap().status, 200);
}

#[test]
fn the_first_ping_is_due_one_interval_after_enable_and_the_first_call_wins() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    assert_eq!(e.poll_timeout(), None);
    e.enable_keepalive(Some(Duration::from_secs(2)), t0);
    e.enable_keepalive(Some(Duration::from_secs(5)), ms(t0, 10));
    assert_eq!(e.poll_timeout(), Some(ms(t0, 2000)));
    e.handle_timeout(ms(t0, 1999), false);
    assert!(e.poll_transmit().is_none());
    e.handle_timeout(ms(t0, 2000), false);
    let p = one(&mut e);
    assert!(p.is_ping);
    assert_eq!(p.txn, None);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 4000)));
}

#[test]
fn without_an_override_the_interval_is_half_the_default_session_timeout() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(None, t0);
    assert_eq!(e.poll_timeout(), Some(t0 + Duration::from_secs(30)));
}

#[test]
fn a_ping_carries_its_cseq_user_agent_session_and_a_preemptive_header() {
    let t0 = Instant::now();
    let mut e = challenged(
        Some(("user", "pass")),
        r#"Digest realm="R", nonce="n1", qop="auth""#,
    );
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    feed(&mut e);
    e.handle_timeout(ms(t0, 1000), false);
    let p = one(&mut e);
    assert!(
        p.bytes
            .starts_with(b"OPTIONS rtsp://cam.example/live RTSP/1.0\r\n")
    );
    let r = sent(&p);
    // The challenge's OPTIONS and its retry took CSeq 1 and 2.
    assert_eq!(r.headers["cseq"], "3");
    assert_eq!(r.headers["user-agent"], "tst-rtp/test");
    assert!(!r.headers.contains_key("session"), "no session yet");
    let a = &r.headers["authorization"];
    assert!(a.contains(&format!("uri=\"{URI}\"")), "{a}");
    assert_eq!(param(a, "nc"), "00000002");

    feed(&mut e);
    run_setup(&mut e, "abc;timeout=60", ms(t0, 1500));
    // The override holds the 1 s cadence; later pings carry the session.
    feed(&mut e);
    e.handle_timeout(ms(t0, 2000), false);
    let r = sent(&one(&mut e));
    // The SETUP took 4.
    assert_eq!(r.headers["cseq"], "5");
    assert_eq!(r.headers["session"], "abc");
}

/// Port of `keepalive_retunes_to_server_advertised_timeout`.
#[test]
fn a_setup_200_records_the_session_and_retunes_the_interval() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(None, t0);
    run_setup(&mut e, "abc;timeout=1", ms(t0, 100));
    assert_eq!(
        e.poll_timeout(),
        Some(ms(t0, 500)),
        "half of 1 s from enable"
    );
    e.handle_timeout(ms(t0, 500), false);
    let r = sent(&one(&mut e));
    assert_eq!(r.headers["session"], "abc");
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
}

#[test]
fn a_retune_past_the_due_time_pings_at_once() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(None, t0);
    run_setup(&mut e, "abc;timeout=1", ms(t0, 900));
    assert_eq!(e.poll_timeout(), Some(ms(t0, 500)));
    e.handle_timeout(ms(t0, 900), false);
    assert!(one(&mut e).is_ping);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1400)));
}

#[test]
fn a_timeout_of_zero_floors_the_interval_at_100_ms() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(None, t0);
    run_setup(&mut e, "abc;timeout=0", t0);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 100)));
}

#[test]
fn a_timeout_of_zero_learned_before_enable_floors_at_100_ms_too() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    run_setup(&mut e, "abc;timeout=0", t0);
    e.enable_keepalive(None, t0);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 100)));
}

#[test]
fn an_interval_override_is_floored_at_1_ms_only() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    run_setup(&mut e, "abc;timeout=0", t0);
    e.enable_keepalive(Some(Duration::ZERO), t0);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1)));
}

#[test]
fn a_timeout_near_u64_max_saturates_without_panicking() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(None, t0);
    run_setup(&mut e, &format!("abc;timeout={}", u64::MAX), t0);
    // The due time is centuries away, or not representable at all.
    let far = t0 + Duration::from_secs(100 * 365 * 24 * 3600);
    assert!(e.poll_timeout().is_none_or(|d| d > far));
    e.handle_timeout(t0 + Duration::from_secs(3600), false);
    assert!(e.poll_transmit().is_none());
}

/// Port of `builder_interval_override_wins_over_server_timeout`.
#[test]
fn an_interval_override_wins_over_the_server_timeout() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(10)), t0);
    run_setup(&mut e, "abc;timeout=1", t0);
    assert_eq!(e.poll_timeout(), Some(t0 + Duration::from_secs(10)));
    e.handle_timeout(ms(t0, 2000), false);
    assert!(e.poll_transmit().is_none());
}

#[test]
fn a_setup_learned_before_enable_sets_the_first_interval() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    run_setup(&mut e, "abc;timeout=4", t0);
    e.enable_keepalive(None, ms(t0, 100));
    assert_eq!(e.poll_timeout(), Some(ms(t0, 2100)));
}

#[test]
fn submitting_a_teardown_clears_the_session_and_ends_the_pings() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    run_setup(&mut e, "abc", t0);
    let mut td = spec(RtspMethod::Teardown, URI);
    td.headers.push(("session", "abc".to_string()));
    let a = e.submit(td, None, ms(t0, 10)).unwrap();
    one(&mut e);
    assert_eq!(e.poll_timeout(), None, "no ping due after a TEARDOWN");
    e.handle_timeout(t0 + Duration::from_secs(10), false);
    assert!(e.poll_transmit().is_none());
    // Whatever the reply.
    e.handle_reply(
        resp(b"RTSP/1.0 500 Internal Server Error\r\nCSeq: 2\r\n\r\n"),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 500);
    e.handle_timeout(t0 + Duration::from_secs(20), false);
    assert!(e.poll_transmit().is_none());
}

#[test]
fn the_cancel_turns_the_keepalive_off_for_good() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
    e.handle_timeout(ms(t0, 1000), true);
    assert!(e.poll_transmit().is_none());
    assert_eq!(e.poll_timeout(), None);
    e.handle_timeout(ms(t0, 5000), false);
    assert!(e.poll_transmit().is_none());
    e.enable_keepalive(Some(Duration::from_secs(1)), ms(t0, 5000));
    assert_eq!(e.poll_timeout(), None);
}

#[test]
fn a_ping_that_fails_to_encode_ends_the_session() {
    let t0 = Instant::now();
    let mut c = cfg(None);
    c.user_agent = "bad\r\nX: y".to_string();
    let mut e = ClientEngine::new(c);
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    assert!(e.poll_transmit().is_none());
    let ev = events(&mut e);
    match ev.as_slice() {
        [
            Event::SessionDead,
            Event::Ended(EndReason::KeepaliveFailed { msg }),
        ] => {
            assert!(
                msg.starts_with("keepalive ping failed to encode: "),
                "{msg}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(e.poll_timeout(), None);
}

/// Like a caller request, a ping that never reaches the wire takes no
/// CSeq: the next request carries the number the ping would have had.
#[test]
fn a_ping_refused_at_encoding_uses_no_cseq() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    // The peer's session id carries a control byte the ping cannot echo.
    setup_over_input(&mut e, "abc\x01", t0);
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    assert!(e.poll_transmit().is_none());
    assert!(matches!(
        events(&mut e).as_slice(),
        [
            Event::SessionDead,
            Event::Ended(EndReason::KeepaliveFailed { .. })
        ]
    ));
    e.submit(spec(RtspMethod::Options, URI), None, ms(t0, 1000))
        .unwrap();
    assert_eq!(cseq(&one(&mut e)), 2);
}

fn pinged(creds: Option<(&str, &str)>) -> (ClientEngine, Instant, u32) {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(creds));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    let c = cseq(&one(&mut e));
    (e, t0, c)
}

#[test]
fn a_ping_200_emits_nothing() {
    let (mut e, t0, c) = pinged(None);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().keepalive_responses, 1);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn a_ping_401_refreshes_the_challenge() {
    let (mut e, t0, c) = pinged(Some(("user", "pass")));
    e.handle_reply(
        resp(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"fresh\"\r\n\r\n"
            )
            .as_bytes(),
        ),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert!(e.poll_transmit().is_none(), "a ping is never retried");
    feed(&mut e);
    e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
    let a = authz(&one(&mut e)).expect("the ping's challenge signs the next request");
    assert_eq!(param(&a, "nonce"), "fresh");
}

#[test]
fn a_ping_454_is_session_expired() {
    let (mut e, t0, c) = pinged(None);
    e.handle_reply(
        resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
        ),
        "{ev:?}"
    );
}

/// A `454` to a ping stops the pings: the session is gone, so later due
/// times queue nothing and end nothing a second time.
#[test]
fn a_ping_454_stops_the_pings() {
    let (mut e, t0, c) = pinged(None);
    e.handle_reply(
        resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        ms(t0, 1010),
    );
    assert_eq!(events(&mut e).len(), 2);
    for n in 2..=5 {
        e.handle_timeout(ms(t0, 1000 * n), false);
        assert!(
            e.poll_transmit().is_none(),
            "no ping after a 454 (at {n} s)"
        );
        assert!(events(&mut e).is_empty(), "no second end (at {n} s)");
    }
}

/// With two pings in flight both answered `454`, the session ends once: the
/// first `454` forgets the other ping, whose reply then matches nothing.
#[test]
fn a_second_ping_454_ends_nothing_again() {
    let (mut e, t0, first) = pinged(None);
    e.handle_timeout(ms(t0, 2000), false);
    let second = cseq(&one(&mut e));
    for c in [first, second] {
        e.handle_reply(
            resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            ms(t0, 2010),
        );
    }
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
        ),
        "{ev:?}"
    );
    assert_eq!(e.counters().keepalive_responses, 1);
    assert_eq!(e.counters().unmatched_replies, 1);
}

/// One ping at a time: while one is still unwritten, a due ping takes no
/// CSeq and queues nothing, and the next falls due one interval later.
#[test]
fn a_deferred_ping_queues_nothing_and_is_due_one_interval_later() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    // Not due yet: nothing to defer.
    e.defer_ping(ms(t0, 500));
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
    e.handle_timeout(ms(t0, 1000), false);
    let first = cseq(&one(&mut e));
    // Its bytes are still unwritten when the second is due, a little late.
    e.defer_ping(ms(t0, 2010));
    e.handle_timeout(ms(t0, 2010), false);
    assert!(transmits(&mut e).is_empty());
    assert!(events(&mut e).is_empty());
    assert_eq!(e.poll_timeout(), Some(ms(t0, 3010)));
    // Written by then: the ping goes out, under the next CSeq.
    e.handle_timeout(ms(t0, 3010), false);
    let second = one(&mut e);
    assert!(second.is_ping);
    assert_eq!(cseq(&second), first + 1);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 4010)));
    // The first ping is still awaited: its reply is a ping's.
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {first}\r\n\r\n").as_bytes()),
        ms(t0, 3020),
    );
    assert_eq!(e.counters().keepalive_responses, 1);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn only_the_latest_unanswered_pings_are_remembered() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    let n = MAX_PINGS_AWAITING_REPLY as u64 + 1;
    for i in 1..=n {
        e.handle_timeout(ms(t0, 1000 * i), false);
    }
    assert_eq!(transmits(&mut e).len(), n as usize);
    // The oldest was forgotten; the next one is still known.
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n"), t0);
    assert_eq!(e.counters().unmatched_replies, 1);
    assert_eq!(e.counters().keepalive_responses, 1);
}

// --- deadlines ---

#[test]
fn a_transaction_past_its_deadline_times_out_once() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e
        .submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 200)), t0)
        .unwrap();
    one(&mut e);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 200)));
    e.handle_timeout(ms(t0, 199), false);
    assert!(events(&mut e).is_empty());
    e.handle_timeout(ms(t0, 200), false);
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
    assert_eq!(e.poll_timeout(), None);
    e.handle_timeout(ms(t0, 300), false);
    assert!(events(&mut e).is_empty());
    // Its late reply matches nothing live.
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), ms(t0, 400));
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 1);
}

#[test]
fn a_reply_at_or_after_the_deadline_is_a_timeout() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e
        .submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 200)), t0)
        .unwrap();
    one(&mut e);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), ms(t0, 200));
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
    e.handle_timeout(ms(t0, 300), false);
    assert!(events(&mut e).is_empty());
}

#[test]
fn the_retry_after_a_401_keeps_the_original_deadline() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    let a = e
        .submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 1000)), t0)
        .unwrap();
    one(&mut e);
    e.handle_reply(resp(BASIC_401), ms(t0, 600));
    one(&mut e);
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
    e.handle_timeout(ms(t0, 1000), false);
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
}

#[test]
fn poll_timeout_is_the_earlier_of_the_ping_and_the_deadline() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 300)), t0)
        .unwrap();
    assert_eq!(e.poll_timeout(), Some(ms(t0, 300)));
    e.submit(spec(RtspMethod::Describe, URI), Some(ms(t0, 3000)), t0)
        .unwrap();
    assert_eq!(e.poll_timeout(), Some(ms(t0, 1000)));
}

// --- unmatched replies ---

#[test]
fn a_reply_matching_nothing_live_is_counted_and_dropped() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n"), t0);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 777\r\n\r\n"), t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 2);
    assert_eq!(e.counters().keepalive_responses, 0);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    // A second reply to the answered transaction is unmatched too.
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 3);
}

// --- the end of the stream ---

fn closed_with(end: StreamEnd) -> (Vec<Event>, ClientEngine, TxnId) {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    let a = e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
    e.handle_closed(end);
    let ev = events(&mut e);
    (ev, e, a)
}

fn assert_closed(
    end: StreamEnd,
    kind: io::ErrorKind,
    dead: bool,
    reason: impl Fn(&EndReason) -> bool,
) {
    let (ev, mut e, a) = closed_with(end);
    let (head, rest) = if dead {
        assert!(matches!(ev.first(), Some(Event::SessionDead)), "{ev:?}");
        (&ev[1..2], &ev[2..])
    } else {
        (&ev[0..1], &ev[1..])
    };
    match head {
        [Event::Ended(r)] => assert!(reason(r), "{r:?}"),
        other => panic!("{other:?} in {ev:?}"),
    }
    match rest {
        [
            Event::Reply {
                txn,
                result: Err(RtspError::Io(k)),
            },
        ] => {
            assert_eq!(*txn, a);
            assert_eq!(*k, kind);
        }
        other => panic!("{other:?} in {ev:?}"),
    }
    assert!(e.poll_transmit().is_none(), "queued bytes are dropped");
    assert_eq!(e.poll_timeout(), None);
    // A later request is answered at once with the same kind.
    assert!(matches!(
        e.submit(spec(RtspMethod::Teardown, URI), None, Instant::now()),
        Err(RtspError::Io(k)) if k == kind
    ));
    assert!(e.poll_transmit().is_none());
    // A second end changes nothing.
    e.handle_closed(StreamEnd::Stopped);
    assert!(e.poll_event().is_none());
}

#[test]
fn a_peer_close_fails_the_request_and_ends_clean() {
    assert_closed(
        StreamEnd::PeerClosed,
        io::ErrorKind::UnexpectedEof,
        true,
        |r| matches!(r, EndReason::CleanTeardown),
    );
}

#[test]
fn a_read_failure_ends_with_transport_failed() {
    assert_closed(
        StreamEnd::ReadFailed(io::Error::from(io::ErrorKind::ConnectionReset)),
        io::ErrorKind::ConnectionReset,
        true,
        |r| matches!(r, EndReason::TransportFailed { msg } if msg.starts_with("TCP read failed: ")),
    );
}

#[test]
fn a_write_failure_ends_with_transport_failed() {
    assert_closed(
        StreamEnd::WriteFailed(io::Error::from(io::ErrorKind::BrokenPipe)),
        io::ErrorKind::BrokenPipe,
        true,
        |r| matches!(r, EndReason::TransportFailed { msg } if msg.starts_with("TCP write failed: ")),
    );
}

#[test]
fn a_stalled_write_ends_by_what_was_stalled() {
    const MSG: &str = "control TCP write made no progress for the write timeout";
    assert_closed(
        StreamEnd::WriteStalled { ping: true },
        io::ErrorKind::TimedOut,
        true,
        |r| matches!(r, EndReason::KeepaliveFailed { msg } if msg == MSG),
    );
    assert_closed(
        StreamEnd::WriteStalled { ping: false },
        io::ErrorKind::TimedOut,
        true,
        |r| matches!(r, EndReason::TransportFailed { msg } if msg == MSG),
    );
}

#[test]
fn a_stop_ends_as_cancelled_without_session_dead() {
    assert_closed(
        StreamEnd::Stopped,
        io::ErrorKind::UnexpectedEof,
        false,
        |r| matches!(r, EndReason::Cancelled),
    );
}

#[test]
fn replies_and_timeouts_after_the_end_do_nothing() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.submit(spec(RtspMethod::Play, URI), Some(ms(t0, 500)), t0)
        .unwrap();
    e.handle_closed(StreamEnd::PeerClosed);
    events(&mut e);
    transmits(&mut e);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), t0);
    e.handle_reply(
        resp(b"RTSP/1.0 454 Session Not Found\r\nCSeq: 777\r\n\r\n"),
        t0,
    );
    e.handle_timeout(t0 + Duration::from_secs(10), false);
    e.handle_timeout(t0 + Duration::from_secs(20), true);
    assert!(e.poll_event().is_none());
    assert!(e.poll_transmit().is_none());
    assert_eq!(e.counters().unmatched_replies, 0);
    assert_eq!(e.counters().keepalive_responses, 0);
}

// --- an accepted TEARDOWN ---

fn teardown_answered(status_line: &str) -> (Vec<Event>, ClientEngine) {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(
        resp(format!("{status_line}\r\nCSeq: 1\r\n\r\n").as_bytes()),
        t0,
    );
    (events(&mut e), e)
}

#[test]
fn an_accepted_teardown_ends_clean_and_the_engine_stays_open() {
    for status in [
        "RTSP/1.0 200 OK",
        "RTSP/1.0 204 No Content",
        "RTSP/1.0 454 Session Not Found",
    ] {
        let (ev, mut e) = teardown_answered(status);
        assert!(
            matches!(
                ev.as_slice(),
                [
                    Event::Ended(EndReason::CleanTeardown),
                    Event::Reply { result: Ok(_), .. }
                ]
            ),
            "{status}: {ev:?}"
        );
        e.submit(spec(RtspMethod::Options, URI), None, Instant::now())
            .unwrap();
        assert_eq!(cseq(&one(&mut e)), 2);
    }
}

#[test]
fn a_refused_teardown_does_not_end() {
    for status in [
        "RTSP/1.0 404 Not Found",
        "RTSP/1.0 500 Internal Server Error",
    ] {
        let (ev, _) = teardown_answered(status);
        assert!(
            matches!(ev.as_slice(), [Event::Reply { result: Ok(_), .. }]),
            "{status}: {ev:?}"
        );
    }
}

#[test]
fn a_non_teardown_454_does_not_end() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Play, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(
        resp(b"RTSP/1.0 454 Session Not Found\r\nCSeq: 1\r\n\r\n"),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 454);
}

// --- a request racing a keepalive ping ---

#[test]
fn a_ping_reply_before_the_request_reply_is_not_the_request_reply() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    let a = e
        .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
        .unwrap();
    let req = cseq(&one(&mut e));
    e.handle_timeout(ms(t0, 1000), false);
    let ping = cseq(&one(&mut e));
    assert_ne!(req, ping);
    // The ping's reply first: consumed, the request still waits.
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {ping}\r\n\r\n").as_bytes()),
        ms(t0, 1010),
    );
    assert!(events(&mut e).is_empty());
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {req}\r\nContent-Length: 3\r\n\r\nv=0").as_bytes()),
        ms(t0, 1020),
    );
    let r = reply(&mut e, a).unwrap();
    assert_eq!(&r.body[..], b"v=0");
    // And a ping answered after the request's reply is still a ping reply.
    e.handle_timeout(ms(t0, 2000), false);
    let ping2 = cseq(&one(&mut e));
    let b = e
        .submit(spec(RtspMethod::Play, URI), None, ms(t0, 2001))
        .unwrap();
    let req2 = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {req2}\r\n\r\n").as_bytes()),
        ms(t0, 2002),
    );
    assert_eq!(reply(&mut e, b).unwrap().status, 200);
    e.handle_reply(
        resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {ping2}\r\n\r\n").as_bytes()),
        ms(t0, 2003),
    );
    assert!(matches!(
        events(&mut e).as_slice(),
        [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
    ));
    assert_eq!(e.counters().keepalive_responses, 2);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn a_ping_reply_between_a_401_and_the_retry_reply_leaves_the_retry_its_own() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    let a = e
        .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
        .unwrap();
    let req = cseq(&one(&mut e));
    e.handle_timeout(ms(t0, 1000), false);
    let ping = cseq(&one(&mut e));
    e.handle_reply(
        resp(
            String::from_utf8_lossy(BASIC_401)
                .replace("CSeq: 1", &format!("CSeq: {req}"))
                .as_bytes(),
        ),
        ms(t0, 1010),
    );
    let retry = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {ping}\r\n\r\n").as_bytes()),
        ms(t0, 1020),
    );
    assert!(
        events(&mut e).is_empty(),
        "the ping's reply is not the retry's"
    );
    e.handle_reply(
        resp(
            format!("RTSP/1.0 200 OK\r\nCSeq: {retry}\r\nContent-Length: 3\r\n\r\nv=0").as_bytes(),
        ),
        ms(t0, 1030),
    );
    assert_eq!(&reply(&mut e, a).unwrap().body[..], b"v=0");
    assert_eq!(e.counters().keepalive_responses, 1);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn a_ping_401_between_request_and_reply_rotates_the_nonce_once() {
    let t0 = Instant::now();
    let mut e = challenged(
        Some(("user", "pass")),
        r#"Digest realm="R", nonce="n1", qop="auth""#,
    );
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    feed(&mut e);
    let a = e
        .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
        .unwrap();
    let req = one(&mut e);
    assert_eq!(param(&authz(&req).unwrap(), "nonce"), "n1");
    feed(&mut e);
    e.handle_timeout(ms(t0, 1000), false);
    let ping = one(&mut e);
    assert_eq!(param(&authz(&ping).unwrap(), "nonce"), "n1");
    const N2_401: &str = "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n2\", qop=\"auth\"\r\n\r\n";
    // The server rotates the nonce: the ping's 401 arrives first...
    e.handle_reply(
        resp(N2_401.replace("{}", &cseq(&ping).to_string()).as_bytes()),
        ms(t0, 1010),
    );
    assert!(events(&mut e).is_empty());
    // ...then the request's, with the same new challenge.
    e.handle_reply(
        resp(N2_401.replace("{}", &cseq(&req).to_string()).as_bytes()),
        ms(t0, 1020),
    );
    feed(&mut e);
    let retry = one(&mut e);
    let ra = authz(&retry).unwrap();
    assert_eq!((param(&ra, "nonce"), param(&ra, "nc")), ("n2", "00000001"));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&retry)).as_bytes()),
        ms(t0, 1030),
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    // The count went on from the retry: the rotation reset it once.
    feed(&mut e);
    e.handle_timeout(ms(t0, 2000), false);
    let pa = authz(&one(&mut e)).unwrap();
    assert_eq!((param(&pa, "nonce"), param(&pa, "nc")), ("n2", "00000002"));
}

#[test]
fn no_nonce_count_is_used_twice() {
    let t0 = Instant::now();
    let mut e = challenged(
        Some(("user", "pass")),
        r#"Digest realm="R", nonce="n1", qop="auth""#,
    );
    e.enable_keepalive(Some(Duration::from_millis(100)), t0);
    let mut seen = std::collections::HashSet::new();
    seen.insert(("n1".to_string(), "00000001".to_string()));
    for round in 0..40u64 {
        let now = ms(t0, 100 * (round + 1));
        // A request and a ping in flight together.
        feed(&mut e);
        e.submit(spec(RtspMethod::GetParameter, URI), None, now)
            .unwrap();
        feed(&mut e);
        e.handle_timeout(now, false);
        let out = transmits(&mut e);
        assert_eq!(out.len(), 2);
        let (req, ping) = if out[0].is_ping {
            (&out[1], &out[0])
        } else {
            (&out[0], &out[1])
        };
        // Every fifth round the server rotates the nonce on the ping
        // AND challenges the request, in either order.
        if round % 5 == 4 {
            let nonce = format!("n{}", round + 2);
            let challenge = |c: u32| {
                resp(
                    format!(
                        "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"{nonce}\", qop=\"auth\"\r\n\r\n"
                    )
                    .as_bytes(),
                )
            };
            if round % 2 == 0 {
                e.handle_reply(challenge(cseq(ping)), now);
                e.handle_reply(challenge(cseq(req)), now);
            } else {
                e.handle_reply(challenge(cseq(req)), now);
                e.handle_reply(challenge(cseq(ping)), now);
            }
            // The request's second attempt, once both were read.
            feed(&mut e);
        }
        for t in out.iter().chain(transmits(&mut e).iter()) {
            if let Some(a) = authz(t) {
                let key = (param(&a, "nonce").to_string(), param(&a, "nc").to_string());
                assert!(seen.insert(key.clone()), "{key:?} used twice");
            }
        }
        events(&mut e);
    }
    assert!(seen.len() > 80, "{}", seen.len());
}

// --- the inbound half: framing, media, RTCP, caps ---

use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T, RtpHeader};
use crate::rtsp::message::{
    MAX_RTSP_BODY_BYTES, MAX_RTSP_MESSAGE_BYTES, RtspFraming, rtsp_frame_decision,
};

fn pull(id: u8) -> SetupRoute {
    SetupRoute {
        id: RouteId(id),
        kind: RouteKind::Pull,
    }
}

fn push(id: u8) -> SetupRoute {
    SetupRoute {
        id: RouteId(id),
        kind: RouteKind::Push,
    }
}

/// An engine fed from an interleaved stream on channels 0/1.
fn interleaved() -> ClientEngine {
    let mut e = ClientEngine::new(cfg(None));
    e.install_route(pull(0), 0, 1).unwrap();
    e
}

/// A minimal valid fixed RTP header (V=2, PT=33).
fn valid_rtp_header() -> [u8; RTP_HEADER_LEN] {
    let mut h = [0u8; RTP_HEADER_LEN];
    RtpHeader::new(0, 0, 0).encode_into(&mut h);
    h
}

/// `body` in one interleaved frame on `channel`.
fn interleaved_frame(channel: u8, body: &[u8]) -> Vec<u8> {
    let mut f = vec![b'$', channel];
    f.extend_from_slice(&u16::try_from(body.len()).unwrap().to_be_bytes());
    f.extend_from_slice(body);
    f
}

fn media(ev: &[Event]) -> Vec<&[u8]> {
    ev.iter()
        .filter_map(|e| match e {
            Event::Media { bytes, .. } => Some(&bytes[..]),
            _ => None,
        })
        .collect()
}

/// The events of a stream closed by a framing violation: `SessionDead`,
/// then `ProtocolError`, then (when one was pending) the request's
/// `BadResponse`. Returns the end message and the reply detail.
fn protocol_end(ev: &[Event]) -> (String, Option<&'static str>) {
    match ev {
        [
            Event::SessionDead,
            Event::Ended(EndReason::ProtocolError { msg }),
        ] => (msg.clone(), None),
        [
            Event::SessionDead,
            Event::Ended(EndReason::ProtocolError { msg }),
            Event::Reply {
                result: Err(RtspError::BadResponse { detail }),
                ..
            },
        ] => (msg.clone(), Some(*detail)),
        other => panic!("expected a protocol end, got {other:?}"),
    }
}

/// The engine is closed: input is ignored and a request is refused.
fn assert_closed_by_protocol_error(e: &mut ClientEngine, t0: Instant) {
    assert!(e.inbuf.is_empty(), "nothing is kept once closed");
    e.handle_input(&interleaved_frame(0, &valid_rtp_header()), t0);
    assert!(e.poll_event().is_none(), "input after the end is ignored");
    assert!(e.inbuf.is_empty());
    assert!(matches!(
        e.submit(spec(RtspMethod::Options, URI), None, t0),
        Err(RtspError::Io(io::ErrorKind::InvalidData))
    ));
    assert!(e.poll_transmit().is_none());
    assert_eq!(e.poll_timeout(), None);
}

/// Port of `rtp_whole_packet_delivered`.
#[test]
fn rtp_whole_packet_delivered() {
    let t0 = Instant::now();
    let header = valid_rtp_header();
    let mut raw = vec![b'$', 0u8, 0x00, 20];
    raw.extend_from_slice(&header);
    raw.extend_from_slice(b"PAYLOAD!");
    let mut e = interleaved();
    e.handle_input(&raw, t0);
    let mut expected = header.to_vec();
    expected.extend_from_slice(b"PAYLOAD!");
    let ev = events(&mut e);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(media(&ev), [expected.as_slice()]);
    assert_eq!(e.counters().rtp_frames, 1);
    assert_eq!(e.counters().malformed_frames, 0);
    assert!(e.inbuf.is_empty());
}

/// Port of `rtcp_frame_routed_to_rtcp_rx`.
#[test]
fn rtcp_frame_routed() {
    let mut raw = vec![b'$', 1u8, 0x00, 4];
    raw.extend_from_slice(b"\xDE\xAD\xBE\xEF");
    let mut e = interleaved();
    e.handle_input(&raw, Instant::now());
    let ev = events(&mut e);
    assert!(
        matches!(ev.as_slice(), [Event::Rtcp { bytes, .. }] if bytes[..] == [0xDE, 0xAD, 0xBE, 0xEF]),
        "{ev:?}"
    );
    assert_eq!(e.counters().rtcp_frames, 1);
}

/// Port of `pump_accepts_full_u16_binary_frame`.
#[test]
fn engine_accepts_full_u16_binary_frame() {
    let mut raw = vec![b'$', 1u8, 0xFF, 0xFF];
    raw.extend(std::iter::repeat_n(0xABu8, 65535));
    let mut e = interleaved();
    e.handle_input(&raw, Instant::now());
    let ev = events(&mut e);
    match ev.as_slice() {
        [Event::Rtcp { bytes: p, .. }] => {
            assert_eq!(p.len(), 65535, "full payload must round-trip intact");
            assert!(p.iter().all(|&b| b == 0xAB));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(e.counters().rtcp_frames, 1);
    assert_eq!(e.counters().malformed_frames, 0);
    assert!(e.poll_event().is_none());
}

/// Port of `rtsp_response_with_body_routed_to_ctrl_rx`: the body is
/// awaited before the reply is complete.
#[test]
fn rtsp_response_with_body() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    transmits(&mut e);
    let raw = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\nHELLO";
    e.handle_input(&raw[..raw.len() - 1], t0);
    assert!(e.poll_event().is_none(), "the body is not complete yet");
    e.handle_input(&raw[raw.len() - 1..], t0);
    assert_eq!(&reply(&mut e, a).unwrap().body[..], b"HELLO");
    assert_eq!(e.counters().rtsp_messages, 1);
    assert!(e.inbuf.is_empty());
}

/// Port of `keepalive_responses_consumed_not_queued`: more ping replies
/// than the old control queue held are consumed, none reaches a caller.
#[test]
fn keepalive_responses_consumed_not_queued() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    let n = 40u64;
    for i in 1..=n {
        e.handle_timeout(ms(t0, 1000 * i), false);
        let c = cseq(&one(&mut e));
        e.handle_input(
            format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes(),
            ms(t0, 1000 * i + 1),
        );
    }
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().keepalive_responses, n);
    assert_eq!(e.counters().rtsp_messages, 0);
    assert_eq!(e.counters().malformed_frames, 0);
    assert_eq!(e.counters().unmatched_replies, 0);
}

/// Port of `keepalive_401_refreshes_shared_auth_challenge`.
#[test]
fn keepalive_401_refreshes_shared_auth_challenge() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.auth.challenge = Some(r#"Digest realm="cam", nonce="old""#.into());
    e.auth.nc = 41;
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    let c = cseq(&one(&mut e));
    e.handle_input(
        format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: \
             Digest realm=\"cam\", nonce=\"rotated\", stale=true\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    assert!(
        e.auth
            .challenge
            .as_deref()
            .is_some_and(|c| c.contains("rotated")),
        "the challenge must be refreshed from the ping's 401, got {:?}",
        e.auth.challenge
    );
    assert_eq!(e.auth.nc, 0, "nonce-count must reset for the new challenge");
    assert_eq!(e.counters().keepalive_responses, 1);
}

/// Port of `keepalive_454_flips_session_dead`.
#[test]
fn keepalive_454_flips_session_dead() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    let c = cseq(&one(&mut e));
    e.handle_input(
        format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes(),
        t0,
    );
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
        ),
        "{ev:?}"
    );
}

/// Port of `rtsp_response_closes_pump_on_malformed_content_length`.
#[test]
fn rtsp_response_closes_engine_on_malformed_content_length() {
    for header in [
        "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: nope\r\n\r\n",
        "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 2000000000\r\n\r\n",
    ] {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.handle_input(header.as_bytes(), t0);
        let (msg, detail) = protocol_end(&events(&mut e));
        assert!(
            msg.starts_with("malformed RTSP Content-Length: "),
            "{header:?}: {msg}"
        );
        assert_eq!(detail, None);
        assert_eq!(e.counters().malformed_frames, 1);
        assert_eq!(e.counters().rtsp_messages, 0);
        assert_closed_by_protocol_error(&mut e, t0);
    }
}

/// Port of `pump_closes_on_unterminated_header_flood`.
#[test]
fn engine_closes_on_unterminated_header_flood() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.handle_input(&vec![b'A'; 128 * 1024], t0);
    let (msg, detail) = protocol_end(&events(&mut e));
    assert!(
        msg.starts_with("control stream buffer exceeded RTSP header/body caps ("),
        "{msg}"
    );
    assert_eq!(detail, None);
    assert_eq!(e.counters().malformed_frames, 1);
    assert_eq!(e.counters().rtsp_messages, 0);
    assert_closed_by_protocol_error(&mut e, t0);
}

/// The same flood dribbled in: the cap holds at 64 KiB of headers.
#[test]
fn a_dribbled_header_flood_is_refused_at_the_cap() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let chunk = [b'A'; 1024];
    let mut fed = 0;
    while e.poll_event().is_none() {
        assert!(fed <= MAX_RTSP_MESSAGE_BYTES, "no end after {fed} bytes");
        e.handle_input(&chunk, t0);
        fed += chunk.len();
    }
    assert_eq!(fed, MAX_RTSP_MESSAGE_BYTES + chunk.len());
    assert!(e.inbuf.is_empty());
}

/// Port of `unknown_channel_frame_counted_and_dropped`.
#[test]
fn unknown_channel_frame_counted_and_dropped() {
    let mut e = interleaved();
    e.handle_input(&[b'$', 7u8, 0x00, 2, 0xAB, 0xCD], Instant::now());
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
    assert_eq!(e.counters().rtp_frames, 0);
    assert_eq!(e.counters().rtcp_frames, 0);
    assert!(e.inbuf.is_empty());
}

/// A SETUP for `route`, submitted and taken off the transmit queue;
/// returns its transaction and CSeq.
fn submit_setup(e: &mut ClientEngine, route: SetupRoute, at: Instant) -> (TxnId, u32) {
    let spec = RequestSpec {
        setup: Some(route),
        ..spec(RtspMethod::Setup, URI)
    };
    let a = e.submit(spec, None, at).unwrap();
    (a, cseq(&one(e)))
}

fn setup_ok(cseq: u32, transport: &str) -> Vec<u8> {
    format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: S1\r\nTransport: {transport}\r\n\r\n")
        .into_bytes()
}

/// A SETUP for `route` answered `200` with `transport`; the reply's outcome.
fn setup_route(
    e: &mut ClientEngine,
    route: SetupRoute,
    transport: &str,
    at: Instant,
) -> Result<ReplyOutcome, RtspError> {
    let (a, c) = submit_setup(e, route, at);
    e.handle_input(&setup_ok(c, transport), at);
    outcome(e, a)
}

fn accepted(route: u8, rtp_channel: u8, rtcp_channel: u8) -> Option<SetupAccepted> {
    Some(SetupAccepted {
        route: RouteId(route),
        rtp_channel,
        rtcp_channel,
    })
}

/// `ev` is one media packet on route `id`.
fn one_media_on(ev: &[Event], id: u8) -> bool {
    matches!(ev, [Event::Media { route, .. }] if *route == RouteId(id))
}

/// `ev` is one RTCP packet on route `id`.
fn one_rtcp_on(ev: &[Event], id: u8) -> bool {
    matches!(ev, [Event::Rtcp { route, .. }] if *route == RouteId(id))
}

/// The events of one valid RTP frame on `channel`.
fn rtp_on(e: &mut ClientEngine, channel: u8) -> Vec<Event> {
    e.handle_input(
        &interleaved_frame(channel, &valid_rtp_header()),
        Instant::now(),
    );
    events(e)
}

/// The events of one RTCP frame on `channel`.
fn rtcp_on(e: &mut ClientEngine, channel: u8) -> Vec<Event> {
    e.handle_input(
        &interleaved_frame(channel, b"\xDE\xAD\xBE\xEF"),
        Instant::now(),
    );
    events(e)
}

/// The channel pair is in place before the bytes behind the reply are
/// framed: frames a server sends with its SETUP reply are delivered,
/// wherever a read splits them.
#[test]
fn a_setup_reply_and_its_frames_in_one_buffer_are_all_delivered() {
    let mut rtp = valid_rtp_header().to_vec();
    rtp.extend_from_slice(b"PAYLOAD!");
    let check = |e: &mut ClientEngine, a: TxnId, at: &str| {
        let ev = events(e);
        match ev.as_slice() {
            [
                Event::Reply { txn, result: Ok(o) },
                Event::Media {
                    route: RouteId(0),
                    bytes: media,
                },
                Event::Rtcp {
                    route: RouteId(0),
                    bytes: rtcp,
                },
            ] => {
                assert_eq!(*txn, a, "{at}");
                assert_eq!(o.setup, accepted(0, 4, 5), "{at}");
                assert_eq!(&media[..], rtp.as_slice(), "{at}");
                assert_eq!(&rtcp[..], b"\xDE\xAD\xBE\xEF", "{at}");
            }
            other => panic!("{at}: {other:?}"),
        }
        assert_eq!(e.counters().malformed_frames, 0, "{at}");
        assert_eq!(e.counters().rtp_frames, 1, "{at}");
        assert_eq!(e.counters().rtcp_frames, 1, "{at}");
        assert!(e.inbuf.is_empty(), "{at}");
    };
    let fresh = || {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let (a, c) = submit_setup(&mut e, pull(0), t0);
        let mut raw = setup_ok(c, "RTP/AVP/TCP;unicast;interleaved=4-5");
        raw.extend(interleaved_frame(4, &rtp));
        raw.extend(interleaved_frame(5, b"\xDE\xAD\xBE\xEF"));
        (e, a, raw, t0)
    };
    let (mut e, a, raw, t0) = fresh();
    e.handle_input(&raw, t0);
    check(&mut e, a, "one buffer");
    for k in 1..raw.len() {
        let (mut e, a, raw, t0) = fresh();
        e.handle_input(&raw[..k], t0);
        e.handle_input(&raw[k..], t0);
        check(&mut e, a, &format!("split at {k}"));
    }
}

#[test]
fn a_frame_before_any_setup_reply_is_counted_and_dropped() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let mut raw = interleaved_frame(0, &valid_rtp_header());
    raw.extend(interleaved_frame(1, b"\xDE\xAD\xBE\xEF"));
    e.handle_input(&raw, t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().malformed_frames, 2);
    assert_eq!(e.counters().rtp_frames, 0);
    assert_eq!(e.counters().rtcp_frames, 0);
    // The stream goes on. A request waiting for its reply installs nothing:
    // the frame ahead of the reply is dropped, the one behind it delivered.
    let (a, c) = submit_setup(&mut e, pull(0), t0);
    let mut raw = interleaved_frame(0, &valid_rtp_header());
    raw.extend(setup_ok(c, "RTP/AVP/TCP;unicast;interleaved=0-1"));
    raw.extend(interleaved_frame(0, &valid_rtp_header()));
    e.handle_input(&raw, t0);
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [
                Event::Reply { txn, result: Ok(_) },
                Event::Media { route: RouteId(0), .. }
            ] if *txn == a
        ),
        "{ev:?}"
    );
    assert_eq!(e.counters().malformed_frames, 3);
    assert_eq!(e.counters().rtp_frames, 1);
}

#[test]
fn a_setup_reply_without_interleaved_installs_0_1() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 0, 1));
    let ev = rtp_on(&mut e, 0);
    assert!(one_media_on(&ev, 0), "{ev:?}");
    let ev = rtcp_on(&mut e, 1);
    assert!(one_rtcp_on(&ev, 0), "{ev:?}");
    assert_eq!(e.counters().malformed_frames, 0);
}

/// The first route delivers on 2/3 and nothing else is routed.
fn assert_only_route_0_on_2_3(e: &mut ClientEngine, unrouted: &[u8]) {
    let before = e.counters().malformed_frames;
    for (n, channel) in (1u64..).zip(unrouted) {
        assert!(rtp_on(e, *channel).is_empty(), "channel {channel}");
        assert_eq!(
            e.counters().malformed_frames,
            before + n,
            "channel {channel}"
        );
    }
    let ev = rtp_on(e, 2);
    assert!(one_media_on(&ev, 0), "{ev:?}");
    let ev = rtcp_on(e, 3);
    assert!(one_rtcp_on(&ev, 0), "{ev:?}");
}

/// A server that names one channel for RTP and RTCP in a pull SETUP reply
/// is played all the same: the route installs, and every frame on that
/// channel is taken as RTP.
#[test]
fn a_pull_setup_reply_naming_one_channel_twice_delivers_its_frames_as_rtp() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=3-3", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 3, 3));
    let ev = rtp_on(&mut e, 3);
    assert!(one_media_on(&ev, 0), "{ev:?}");
    // A report on that channel is read as RTP too: there is no RTCP
    // channel. One too short for an RTP header, as this one is, counts as
    // malformed; a whole sender report passes the header and is handed
    // on, and the receiver drops it by its payload type.
    assert!(rtcp_on(&mut e, 3).is_empty());
    assert_eq!(e.counters().rtp_frames, 2);
    assert_eq!(e.counters().rtcp_frames, 0);
    assert_eq!(e.counters().malformed_frames, 1);
    // The one channel is held like any other: a second route cannot name
    // it.
    for pair in ["3-3", "3-4", "2-3"] {
        let transport = format!("RTP/AVP/TCP;unicast;interleaved={pair}");
        let r = setup_route(&mut e, pull(1), &transport, t0);
        assert!(
            matches!(r, Err(RtspError::BadResponse { .. })),
            "{pair}: {r:?}"
        );
    }
    let ev = rtp_on(&mut e, 3);
    assert!(one_media_on(&ev, 0), "{ev:?}");
}

/// A route this side sends on needs its two channels: the RTP channel is
/// written, the RTCP channel is read.
#[test]
fn a_push_setup_reply_with_equal_channels_is_bad_response() {
    let mut e = announced(1);
    let r = exchange(
        &mut e,
        record_setup(0),
        "200 OK",
        "Transport: RTP/AVP/TCP;unicast;interleaved=5-5;mode=record\r\nSession: X\r\n",
    );
    assert!(
        matches!(
            r,
            Err(RtspError::BadResponse {
                detail: "SETUP reply names one interleaved channel for RTP and RTCP"
            })
        ),
        "{r:?}"
    );
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
    // Nothing is installed.
    assert!(rtcp_on(&mut e, 5).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
    assert!(e.install_route(push(0), 5, 5).is_err());
}

#[test]
fn a_setup_reply_with_a_colliding_pair_is_bad_response() {
    // The second route's RTCP on the first's RTP, its RTP on the first's
    // RTCP, and the same pair.
    for (pair, free) in [("1-2", Some(1)), ("3-4", Some(4)), ("2-3", None)] {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let first = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=2-3", t0);
        assert_eq!(first.unwrap().setup, accepted(0, 2, 3));
        let transport = format!("RTP/AVP/TCP;unicast;interleaved={pair}");
        let r = setup_route(&mut e, pull(1), &transport, t0);
        assert!(
            matches!(r, Err(RtspError::BadResponse { .. })),
            "{pair}: {r:?}"
        );
        // Nothing of the refused pair is installed.
        assert_only_route_0_on_2_3(&mut e, free.as_slice());
    }
}

#[test]
fn a_setup_reply_without_transport_is_bad_response() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let (a, c) = submit_setup(&mut e, pull(0), t0);
    e.handle_input(
        format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\nSession: S1\r\n\r\n").as_bytes(),
        t0,
    );
    let r = outcome(&mut e, a);
    assert!(
        matches!(
            r,
            Err(RtspError::BadResponse {
                detail: "SETUP 200 missing Transport: header"
            })
        ),
        "{r:?}"
    );
    assert!(rtp_on(&mut e, 0).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
}

/// A channel pair that does not parse is refused as the transport parser
/// refuses it.
#[test]
fn a_setup_reply_with_an_unparseable_pair_installs_nothing() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let r = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=1-0", t0);
    assert!(matches!(r, Err(RtspError::UnsupportedTransport)), "{r:?}");
    assert!(rtp_on(&mut e, 1).is_empty());
    assert!(rtp_on(&mut e, 0).is_empty());
    assert_eq!(e.counters().malformed_frames, 2);
}

#[test]
fn a_udp_setup_reply_installs_nothing() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let o = setup_route(
        &mut e,
        pull(0),
        "RTP/AVP;unicast;client_port=5004-5005;server_port=6000-6001",
        t0,
    )
    .unwrap();
    assert_eq!(o.setup, None);
    assert_eq!(o.response.status, 200);
    assert!(rtp_on(&mut e, 0).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
    // The reply is taken all the same: the next ping names its session.
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    assert_eq!(sent(&one(&mut e)).headers["session"], "S1");
}

/// A SETUP that names no route is not read for a channel pair.
#[test]
fn a_setup_without_a_route_installs_nothing() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Setup, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_input(&setup_ok(c, "RTP/AVP/TCP;unicast;interleaved=0-1"), t0);
    assert_eq!(outcome(&mut e, a).unwrap().setup, None);
    assert!(rtp_on(&mut e, 0).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
}

/// On a route this side sends on, a frame the peer sends on the RTP
/// channel is dropped whatever it holds; the RTCP channel carries the
/// peer's reports.
#[test]
fn a_frame_on_a_push_rtp_channel_is_counted_and_dropped() {
    let t0 = Instant::now();
    let mut e = announced(2);
    set_up(&mut e, 0).unwrap();
    let o = set_up(&mut e, 1).unwrap();
    assert_eq!(o.setup, accepted(1, 2, 3));
    let ev = rtp_on(&mut e, 2);
    assert!(matches!(ev.as_slice(), [Event::InboundDropped]), "{ev:?}");
    e.handle_input(&interleaved_frame(2, b"not RTP"), t0);
    let ev = events(&mut e);
    assert!(matches!(ev.as_slice(), [Event::InboundDropped]), "{ev:?}");
    assert_eq!(e.counters().rtp_frames, 2);
    assert_eq!(e.counters().malformed_frames, 0);
    let ev = rtcp_on(&mut e, 3);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::Rtcp { route: RouteId(1), bytes }] if bytes[..] == [0xDE, 0xAD, 0xBE, 0xEF]
        ),
        "{ev:?}"
    );
    assert_eq!(e.counters().rtcp_frames, 1);
}

#[test]
fn a_fifth_route_is_refused() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    for id in 0..4u8 {
        let transport = format!("RTP/AVP/TCP;unicast;interleaved={}-{}", 2 * id, 2 * id + 1);
        let o = setup_route(&mut e, pull(id), &transport, t0).unwrap();
        assert_eq!(o.setup, accepted(id, 2 * id, 2 * id + 1));
    }
    let r = setup_route(&mut e, pull(4), "RTP/AVP/TCP;unicast;interleaved=8-9", t0);
    assert!(matches!(r, Err(RtspError::BadResponse { .. })), "{r:?}");
    assert!(e.install_route(push(4), 8, 9).is_err());
    assert!(e.install_route(pull(u8::MAX), 8, 9).is_err());
    assert!(rtcp_on(&mut e, 9).is_empty(), "nothing is installed");
    assert_eq!(e.counters().malformed_frames, 1);
    // The four routes are untouched.
    for id in 0..4u8 {
        let ev = rtcp_on(&mut e, 2 * id + 1);
        assert!(one_rtcp_on(&ev, id), "{ev:?}");
    }
}

#[test]
fn a_new_pull_setup_replaces_route_0() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=0-1", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 0, 1));
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=2-3", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 2, 3));
    let ev = rtp_on(&mut e, 2);
    assert!(one_media_on(&ev, 0), "{ev:?}");
    // The first pair is gone.
    assert!(rtp_on(&mut e, 0).is_empty());
    assert!(rtcp_on(&mut e, 1).is_empty());
    assert_eq!(e.counters().malformed_frames, 2);
    // A route may take channels of its own previous pair.
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=3-4", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 3, 4));
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=3-4", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 3, 4));
    let ev = rtp_on(&mut e, 3);
    assert!(one_media_on(&ev, 0), "{ev:?}");
}

/// The retry after a `401` is the same SETUP: its `200` installs the route.
#[test]
fn a_setup_retried_after_a_401_installs_its_route() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    let (a, c) = submit_setup(&mut e, pull(0), t0);
    e.handle_input(
        format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert!(rtp_on(&mut e, 0).is_empty(), "a 401 installs nothing");
    let retry = cseq(&one(&mut e));
    let mut raw = setup_ok(retry, "RTP/AVP/TCP;unicast;interleaved=0-1");
    raw.extend(interleaved_frame(0, &valid_rtp_header()));
    e.handle_input(&raw, t0);
    let ev = events(&mut e);
    match ev.as_slice() {
        [
            Event::Reply { txn, result: Ok(o) },
            Event::Media {
                route: RouteId(0), ..
            },
        ] => {
            assert_eq!(*txn, a);
            assert_eq!(o.setup, accepted(0, 0, 1));
        }
        other => panic!("{other:?}"),
    }
}

/// Any `2xx` to a push SETUP is an accepted SETUP: the session is learned
/// and the route installed. A refusal does neither.
#[test]
fn a_setup_reply_of_any_2xx_is_accepted() {
    let t0 = Instant::now();
    let mut e = announced(1);
    let (a, c) = submit_setup(&mut e, push(0), t0);
    e.handle_input(
        format!(
            "RTSP/1.0 461 Unsupported Transport\r\nCSeq: {c}\r\nSession: refused\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    let o = outcome(&mut e, a).unwrap();
    assert_eq!((o.response.status, o.setup), (461, None));
    assert!(rtp_on(&mut e, 0).is_empty(), "a refusal installs nothing");

    let (a, c) = submit_setup(&mut e, push(0), t0);
    e.handle_input(
        format!(
            "RTSP/1.0 201 Created\r\nCSeq: {c}\r\nSession: made\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    let o = outcome(&mut e, a).unwrap();
    assert_eq!((o.response.status, o.setup), (201, accepted(0, 0, 1)));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    assert_eq!(sent(&one(&mut e)).headers["session"], "made");
}

/// The pull side takes exactly `200` with a `Session`: a later pull SETUP
/// it refuses must not move the live session's route.
#[test]
fn a_pull_setup_reply_of_201_installs_nothing_and_leaves_route_0() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let o = setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=0-1", t0).unwrap();
    assert_eq!(o.setup, accepted(0, 0, 1));
    let (a, c) = submit_setup(&mut e, pull(0), t0);
    e.handle_input(
        format!(
            "RTSP/1.0 201 Created\r\nCSeq: {c}\r\nSession: S2\r\n\
             Transport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    let o = outcome(&mut e, a).unwrap();
    assert_eq!((o.response.status, o.setup), (201, None));
    let ev = rtp_on(&mut e, 0);
    assert!(one_media_on(&ev, 0), "{ev:?}");
    assert!(rtp_on(&mut e, 2).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
}

#[test]
fn a_pull_setup_200_without_a_session_header_installs_nothing() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let (a, c) = submit_setup(&mut e, pull(0), t0);
    e.handle_input(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: {c}\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    let o = outcome(&mut e, a).unwrap();
    assert_eq!((o.response.status, o.setup), (200, None));
    assert!(rtp_on(&mut e, 0).is_empty());
    assert_eq!(e.counters().malformed_frames, 1);
}

/// Port of `undersized_rtp_frame_counted_and_dropped`.
#[test]
fn undersized_rtp_frame_counted_and_dropped() {
    let mut e = interleaved();
    e.handle_input(
        &[b'$', 0u8, 0x00, 4, 0x00, 0x00, 0x00, 0x00],
        Instant::now(),
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().rtp_frames, 1);
    assert_eq!(e.counters().malformed_frames, 1);
}

/// Port of `pump_skips_a_non_utf8_header_block_after_a_binary_frame`,
/// with no request waiting.
#[test]
fn engine_skips_a_non_utf8_header_block_after_a_binary_frame() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    let c = cseq(&one(&mut e));
    let mut raw = interleaved_frame(0, &valid_rtp_header());
    raw.extend_from_slice(b"RTSP/1.0 200 OK\r\nX: \xff\r\n\r\n");
    raw.extend_from_slice(
        format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\nContent-Length: 5\r\n\r\nHELLO")
            .as_bytes(),
    );
    e.handle_input(&raw, t0);
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [
                Event::Media { .. },
                Event::SessionDead,
                Event::Ended(EndReason::SessionExpired)
            ]
        ),
        "the reply after the skipped block still arrives: {ev:?}"
    );
    assert_eq!(e.counters().malformed_frames, 1);
    assert_eq!(e.counters().keepalive_responses, 1);
    assert!(e.inbuf.is_empty());
}

/// Mirror of `non_utf8_reply_headers_are_rejected_with_their_own_detail`:
/// the waiting request gets the detail and the stream goes on.
#[test]
fn non_utf8_headers_answer_a_waiting_request_and_the_stream_goes_on() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    transmits(&mut e);
    let mut raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nX: \xff\r\n\r\n".to_vec();
    raw.extend(interleaved_frame(0, &valid_rtp_header()));
    e.handle_input(&raw, t0);
    let ev = events(&mut e);
    match ev.as_slice() {
        [
            Event::Reply {
                txn,
                result:
                    Err(RtspError::BadResponse {
                        detail: "non-UTF8 RTSP headers",
                    }),
            },
            Event::Media { .. },
        ] => assert_eq!(*txn, a),
        other => panic!("{other:?}"),
    }
    assert_eq!(e.counters().malformed_frames, 1);
    let b = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n", t0);
    assert_eq!(reply(&mut e, b).unwrap().status, 200);
}

/// Mirror of `unterminated_reply_headers_are_rejected`, whole and
/// in read-sized slices.
#[test]
fn unterminated_headers_answer_a_waiting_request_and_close() {
    let mut raw = b"RTSP/1.0 200 OK\r\nX-Junk: ".to_vec();
    raw.extend(std::iter::repeat_n(b'A', 128 * 1024));
    for slice in [raw.len(), 4096] {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        for c in raw.chunks(slice) {
            e.handle_input(c, t0);
        }
        let ev = events(&mut e);
        let (msg, detail) = protocol_end(&ev);
        assert!(msg.starts_with("control stream buffer exceeded"), "{msg}");
        assert_eq!(detail, Some("RTSP response headers exceed maximum"));
        assert!(matches!(ev.last(), Some(Event::Reply { txn, .. }) if *txn == a));
        assert_closed_by_protocol_error(&mut e, t0);
    }
}

/// Mirror of `an_oversized_declared_reply_body_is_rejected`.
#[test]
fn an_oversized_declared_body_answers_a_waiting_request_and_closes() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    e.handle_input(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
            MAX_RTSP_BODY_BYTES + 1
        )
        .as_bytes(),
        t0,
    );
    let (msg, detail) = protocol_end(&events(&mut e));
    assert_eq!(
        msg,
        "malformed RTSP Content-Length: Content-Length exceeds maximum"
    );
    assert_eq!(detail, Some("Content-Length exceeds maximum"));
    assert_closed_by_protocol_error(&mut e, t0);
}

/// A body as large as the cap allows is awaited, not refused.
#[test]
fn a_body_at_the_cap_is_awaited_in_full() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    let mut raw =
        format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {MAX_RTSP_BODY_BYTES}\r\n\r\n")
            .into_bytes();
    raw.extend(std::iter::repeat_n(b'v', MAX_RTSP_BODY_BYTES));
    for c in raw.chunks(64 * 1024) {
        e.handle_input(c, t0);
    }
    assert_eq!(reply(&mut e, a).unwrap().body.len(), MAX_RTSP_BODY_BYTES);
    assert!(e.inbuf.is_empty());
}

/// Port of `interleaved_rtp_complex_header_delivered_as_whole_packet`.
#[test]
fn interleaved_rtp_complex_header_delivered_as_whole_packet() {
    // CC=1, X=1, P=1: CSRC, one extension word, 4 payload bytes and 2
    // padding bytes.
    let mut rtp = vec![0xB1, RTP_PT_MP2T];
    rtp.extend_from_slice(&[0u8; 10]);
    rtp.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
    rtp.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]);
    rtp.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
    rtp.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    rtp.extend_from_slice(&[0x00, 0x02]);
    let mut e = interleaved();
    e.handle_input(&interleaved_frame(0, &rtp), Instant::now());
    assert_eq!(media(&events(&mut e)), [rtp.as_slice()]);
    assert_eq!(e.counters().rtp_frames, 1);
    assert_eq!(e.counters().malformed_frames, 0);
}

/// Port of `interleaved_malformed_rtp_dropped_and_counted`.
#[test]
fn interleaved_malformed_rtp_dropped_and_counted() {
    // X=1 but the extension header is missing.
    let rtp = vec![0x90, RTP_PT_MP2T, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut e = interleaved();
    e.handle_input(&interleaved_frame(0, &rtp), Instant::now());
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().rtp_frames, 1);
    assert_eq!(e.counters().malformed_frames, 1);
}

#[test]
fn a_frame_split_at_any_offset_is_delivered_once() {
    let mut rtp = valid_rtp_header().to_vec();
    rtp.extend_from_slice(b"PAYLOAD!");
    let mut raw = interleaved_frame(0, &rtp);
    raw.extend_from_slice(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nv=0");
    let check = |e: &mut ClientEngine, a: TxnId, at: &str| {
        let ev = events(e);
        match ev.as_slice() {
            [
                Event::Media { bytes, .. },
                Event::Reply { txn, result: Ok(r) },
            ] => {
                assert_eq!(&bytes[..], rtp.as_slice(), "{at}");
                assert_eq!(*txn, a, "{at}");
                assert_eq!(&r.response.body[..], b"v=0", "{at}");
            }
            other => panic!("{at}: {other:?}"),
        }
        assert_eq!(e.counters().rtp_frames, 1, "{at}");
        assert_eq!(e.counters().rtsp_messages, 1, "{at}");
        assert_eq!(e.counters().malformed_frames, 0, "{at}");
        assert!(e.inbuf.is_empty(), "{at}");
    };
    for k in 0..=raw.len() {
        let t0 = Instant::now();
        let mut e = interleaved();
        let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
        e.handle_input(&raw[..k], t0);
        e.handle_input(&raw[k..], t0);
        check(&mut e, a, &format!("split at {k}"));
    }
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Describe, URI), None, t0).unwrap();
    for b in &raw {
        e.handle_input(std::slice::from_ref(b), t0);
    }
    check(&mut e, a, "one byte at a time");
}

#[test]
fn ten_thousand_unmatched_replies_are_counted_and_keep_no_memory() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let one_reply = b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n";
    let flood: Vec<u8> = one_reply.repeat(5_000);
    e.handle_input(&flood, t0);
    for _ in 0..5_000 {
        e.handle_input(one_reply, t0);
    }
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 10_000);
    assert_eq!(e.counters().rtsp_messages, 10_000);
    assert!(e.inbuf.is_empty());
    // The request is still waiting for its own reply.
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

/// An RTCP flood ends nothing and keeps nothing back: every frame is
/// counted and handed on, and the waiting request still gets its reply.
#[test]
fn ten_thousand_rtcp_frames_are_counted_and_end_nothing() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let one_frame = interleaved_frame(1, b"\xDE\xAD\xBE\xEF");
    e.handle_input(&one_frame.repeat(5_000), t0);
    for _ in 0..5_000 {
        e.handle_input(&one_frame, t0);
    }
    let ev = events(&mut e);
    assert_eq!(ev.len(), 10_000);
    assert!(ev.iter().all(|e| matches!(e, Event::Rtcp { .. })));
    assert_eq!(e.counters().rtcp_frames, 10_000);
    assert!(e.inbuf.is_empty());
    assert!(!e.is_closed());
    // The request is still waiting for its own reply.
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

/// A `454` whose CSeq no ping carried (here the next one a ping would
/// take) is not a ping's: it ends nothing.
#[test]
fn a_reply_to_a_ping_never_sent_is_dropped() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    e.handle_input(b"RTSP/1.0 454 Session Not Found\r\nCSeq: 2\r\n\r\n", t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 1);
    assert_eq!(e.counters().keepalive_responses, 0);
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

#[test]
fn a_server_request_never_fails_a_pending_request() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let mut raw =
        b"ANNOUNCE rtsp://cam.example/live RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 3\r\n\r\nv=0"
            .to_vec();
    raw.extend_from_slice(b"OPTIONS * RTSP/1.0\r\n\r\n");
    raw.extend_from_slice(b"GET_PARAMETER rtsp://cam.example/live RTSP/1.0\r\nCSeq: x\r\n\r\n");
    e.handle_input(&raw, t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 3);
    assert_eq!(e.counters().rtsp_messages, 3);
    assert!(e.inbuf.is_empty());
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

#[test]
fn a_response_without_a_cseq_goes_to_the_pending_request() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    e.install_route(pull(0), 0, 1).unwrap();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    one(&mut e);
    // The 401 retry runs for a reply without a CSeq too.
    e.handle_input(
        b"RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n",
        t0,
    );
    assert!(events(&mut e).is_empty());
    let retry = one(&mut e);
    assert!(authz(&retry).is_some());
    e.handle_input(b"RTSP/1.0 200 OK\r\nPublic: OPTIONS, DESCRIBE\r\n\r\n", t0);
    let ok = reply(&mut e, a).unwrap();
    assert!(ok.headers["public"].contains("DESCRIBE"));
    assert_eq!(e.counters().unmatched_replies, 0);
    assert_eq!(e.counters().rtsp_messages, 2);
}

#[test]
fn a_response_without_a_cseq_and_nothing_pending_is_counted() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.handle_input(b"RTSP/1.0 200 OK\r\n\r\n", t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 1);
    // A CSeq that is present but matches nothing stays unmatched, and
    // the waiting request keeps waiting.
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 7\r\n\r\n", t0);
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: x\r\n\r\n", t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 3);
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n", t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    // Once answered, a reply without a CSeq is unmatched again.
    e.handle_input(b"RTSP/1.0 200 OK\r\n\r\n", t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 4);
}

#[test]
fn an_unparseable_response_fails_the_pending_request_with_the_parse_error() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let bad = b"RTSP/1.0 abc OK\r\nCSeq: 1\r\n\r\n";
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let mut raw = bad.to_vec();
    raw.extend(interleaved_frame(0, &valid_rtp_header()));
    e.handle_input(&raw, t0);
    let ev = events(&mut e);
    match ev.as_slice() {
        [
            Event::Reply {
                txn,
                result:
                    Err(RtspError::BadResponse {
                        detail: "non-numeric status code",
                    }),
            },
            Event::Media { .. },
        ] => assert_eq!(*txn, a),
        other => panic!("{other:?}"),
    }
    assert_eq!(e.counters().unmatched_replies, 0);
    // With nothing waiting it is counted, and the stream goes on.
    e.handle_input(bad, t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 1);
    let b = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    e.handle_input(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n", t0);
    assert_eq!(reply(&mut e, b).unwrap().status, 200);
}

/// Answers a SETUP through the input path.
fn setup_over_input(e: &mut ClientEngine, session: &str, at: Instant) {
    let a = e.submit(spec(RtspMethod::Setup, URI), None, at).unwrap();
    let c = cseq(&one(e));
    e.handle_input(
        format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\nSession: {session}\r\n\r\n").as_bytes(),
        at,
    );
    assert_eq!(reply(e, a).unwrap().status, 200);
}

#[test]
fn a_session_timeout_near_u64_max_does_not_wrap_the_interval() {
    for enable_first in [true, false] {
        let t0 = Instant::now();
        let mut e = interleaved();
        if enable_first {
            e.enable_keepalive(None, t0);
        }
        setup_over_input(&mut e, &format!("abc;timeout={}", u64::MAX), t0);
        if !enable_first {
            e.enable_keepalive(None, t0);
        }
        let far = t0 + Duration::from_secs(100 * 365 * 24 * 3600);
        assert!(
            e.poll_timeout().is_none_or(|d| d > far),
            "enable first: {enable_first}"
        );
        e.handle_timeout(t0 + Duration::from_secs(3600), false);
        assert!(e.poll_transmit().is_none());
    }
}

#[test]
fn timeout_zero_floors_the_interval_at_100_ms() {
    for enable_first in [true, false] {
        let t0 = Instant::now();
        let mut e = interleaved();
        if enable_first {
            e.enable_keepalive(None, t0);
        }
        setup_over_input(&mut e, "abc;timeout=0", t0);
        if !enable_first {
            e.enable_keepalive(None, t0);
        }
        assert_eq!(
            e.poll_timeout(),
            Some(ms(t0, 100)),
            "enable first: {enable_first}"
        );
    }
}

#[test]
fn input_after_the_stream_ended_is_ignored() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.handle_input(b"RTSP/1.0 200 OK\r\n", t0);
    e.handle_closed(StreamEnd::PeerClosed);
    events(&mut e);
    assert!(e.inbuf.is_empty());
    e.handle_input(&interleaved_frame(0, &valid_rtp_header()), t0);
    assert!(e.poll_event().is_none());
    assert_eq!(e.counters().rtp_frames, 0);
    assert!(e.inbuf.is_empty());
}

#[test]
fn an_accepted_teardown_does_not_stop_the_input() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    let mut raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n".to_vec();
    raw.extend(interleaved_frame(0, &valid_rtp_header()));
    e.handle_input(&raw, t0);
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [
                Event::Ended(EndReason::CleanTeardown),
                Event::Reply { txn, result: Ok(_) },
                Event::Media { .. }
            ] if *txn == a
        ),
        "{ev:?}"
    );
}

#[test]
fn the_fuzz_hook_returns_on_hostile_input() {
    let mut flood = vec![b'A'; 70 * 1024];
    flood.extend_from_slice(b"\r\n\r\n");
    for data in [
        &b""[..],
        b"$",
        b"$\x00\xff\xff",
        b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: nope\r\n\r\n",
        b"RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Digest\r\n\r\n",
        b"RTSP/1.0 401 Unauthorized\r\nCSeq: 2\r\nWWW-Authenticate: Digest\r\n\r\n",
        b"RTSP/1.0 454 Session Not Found\r\nCSeq: 2\r\n\r\nRTSP/1.0 200 OK\r\n\r\n",
        b"RTSP/1.0 100 Continue\r\nCSeq: 1\r\n\r\nRTSP/1.0 100 Continue\r\nCSeq: 2\r\n\r\n",
        b"\xff\xfe\r\n\r\n$\x01\x00\x00",
        &flood,
    ] {
        crate::rtsp::client::fuzz_client_engine_input(data);
    }
}

// --- the header terminator is looked for in new bytes only ---

fn same_events(a: &[Event], b: &[Event]) -> bool {
    fn key(e: &Event) -> String {
        match e {
            Event::Reply { txn, result } => match result {
                Ok(o) => {
                    let r = &o.response;
                    let mut h: Vec<_> = r.headers.iter().collect();
                    h.sort();
                    format!(
                        "reply {txn:?} {} {h:?} {:?} {:?}",
                        r.status,
                        &r.body[..],
                        o.setup
                    )
                }
                Err(err) => format!("reply {txn:?} {err:?}"),
            },
            Event::Media { route, bytes } => format!("media {route:?} {:?}", &bytes[..]),
            Event::Rtcp { route, bytes } => format!("rtcp {route:?} {:?}", &bytes[..]),
            other => format!("{other:?}"),
        }
    }
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| key(x) == key(y))
}

/// Every counter but `frame_decisions`.
fn counted(e: &ClientEngine) -> [u64; 6] {
    let c = e.counters();
    [
        c.rtsp_messages,
        c.rtp_frames,
        c.rtcp_frames,
        c.malformed_frames,
        c.keepalive_responses,
        c.unmatched_replies,
    ]
}

#[test]
fn a_dribbled_header_is_decided_once() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let mut head = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 300\r\nX-Long: ".to_vec();
    let value_at = head.len();
    head.resize(60 * 1024, b'v');
    for b in &head {
        e.handle_input(std::slice::from_ref(b), t0);
    }
    assert!(e.poll_event().is_none());
    e.handle_input(b"\r\n\r\n", t0);
    assert!(e.poll_event().is_none(), "the body is awaited");
    for _ in 0..300 {
        e.handle_input(b"b", t0);
    }
    let r = reply(&mut e, a).unwrap();
    assert_eq!(r.body.len(), 300);
    assert_eq!(r.headers["x-long"].len(), 60 * 1024 - value_at);
    assert!(
        e.counters().frame_decisions <= 3,
        "{}",
        e.counters().frame_decisions
    );
    assert!(e.inbuf.is_empty());
}

#[test]
fn split_invariance_holds_below_the_header_cap() {
    let mut raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 300\r\nX-Pad: ".to_vec();
    raw.resize(2048, b'p');
    raw.extend_from_slice(b"\r\n\r\n");
    raw.extend(std::iter::repeat_n(b'b', 300));
    let mut rtp = valid_rtp_header().to_vec();
    rtp.extend_from_slice(b"PAYLOAD!");
    raw.extend(interleaved_frame(0, &rtp));
    let run = |slice: usize| {
        let t0 = Instant::now();
        let mut e = interleaved();
        e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        for c in raw.chunks(slice) {
            e.handle_input(c, t0);
        }
        let ev = events(&mut e);
        assert!(e.inbuf.is_empty(), "slice {slice}");
        (ev, e)
    };
    let (whole_ev, whole) = run(raw.len());
    let (bytes_ev, bytes) = run(1);
    assert!(
        matches!(
            whole_ev.as_slice(),
            [Event::Reply { result: Ok(_), .. }, Event::Media { .. }]
        ),
        "{whole_ev:?}"
    );
    assert!(
        same_events(&whole_ev, &bytes_ev),
        "{whole_ev:?} vs {bytes_ev:?}"
    );
    assert_eq!(counted(&whole), counted(&bytes));
}

#[test]
fn the_unterminated_header_cap_has_not_moved() {
    // The largest unterminated length the shared decision still waits on.
    let need_more = |n: usize| rtsp_frame_decision(&vec![b'A'; n]) == RtspFraming::NeedMore;
    let (mut lo, mut hi) = (1usize, 4 * MAX_RTSP_MESSAGE_BYTES);
    assert!(need_more(lo) && !need_more(hi));
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if need_more(mid) { lo = mid } else { hi = mid }
    }
    let last_waiting = lo;
    for slice in [last_waiting, 1024, 1] {
        let t0 = Instant::now();
        let mut e = interleaved();
        for c in vec![b'A'; last_waiting].chunks(slice) {
            e.handle_input(c, t0);
        }
        assert!(e.poll_event().is_none(), "slice {slice}");
        assert_eq!(e.inbuf.len(), last_waiting);
        e.handle_input(b"A", t0);
        let (msg, _) = protocol_end(&events(&mut e));
        assert_eq!(
            msg,
            format!(
                "control stream buffer exceeded RTSP header/body caps ({} bytes buffered)",
                last_waiting + 1
            ),
            "slice {slice}"
        );
    }
}

#[test]
fn a_partial_header_after_a_complete_message_is_scanned_from_its_own_start() {
    let t0 = Instant::now();
    let mut e = interleaved();
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    // A whole unmatched reply, then the start of the request's reply,
    // whose terminator straddles the next two reads.
    e.handle_input(
        b"RTSP/1.0 200 OK\r\nCSeq: 9\r\n\r\nRTSP/1.0 200 OK\r\nCSeq: 1\r",
        t0,
    );
    assert!(e.poll_event().is_none());
    e.handle_input(b"\n\r", t0);
    assert!(e.poll_event().is_none());
    e.handle_input(b"\n", t0);
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    assert_eq!(e.counters().unmatched_replies, 1);
    assert!(e.inbuf.is_empty());
}

// --- request bodies ---

fn sdp_body() -> Vec<u8> {
    (0..120u8).map(|i| b'a' + i % 26).collect()
}

#[test]
fn announce_carries_content_type_content_length_and_the_body() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let body = sdp_body();
    e.submit(with_sdp(RtspMethod::Announce, &body), None, t0)
        .unwrap();
    let r = sent(&one(&mut e));
    assert_eq!(r.method, RtspMethod::Announce);
    assert_eq!(r.headers["content-type"], "application/sdp");
    assert_eq!(r.headers["content-length"], "120");
    assert_eq!(&r.body[..], &body[..]);
}

#[test]
fn the_401_retry_resends_the_identical_body() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    let body = sdp_body();
    let a = e
        .submit(with_sdp(RtspMethod::Announce, &body), None, t0)
        .unwrap();
    let first = one(&mut e);
    let c = cseq(&first);
    e.handle_reply(
        resp(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n1\"\r\n\r\n"
            )
            .as_bytes(),
        ),
        t0,
    );
    feed(&mut e);
    assert!(events(&mut e).is_empty());
    let retry = one(&mut e);
    assert_eq!(cseq(&retry), c + 1);
    assert!(authz(&first).is_none());
    assert!(authz(&retry).is_some_and(|a| a.starts_with("Digest ")));
    let (f, r) = (sent(&first), sent(&retry));
    assert_eq!(r.headers["content-type"], "application/sdp");
    assert_eq!(r.headers["content-length"], "120");
    assert_eq!(&r.body[..], &body[..]);
    assert_eq!(r.body, f.body);
    assert!(retry.bytes.ends_with(&body));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", c + 1).as_bytes()),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a header the engine writes itself")]
fn a_spec_carrying_content_type_trips_the_debug_assert() {
    let mut bad = with_sdp(RtspMethod::Announce, b"v=0");
    bad.headers
        .push(("Content-Type", "application/sdp".to_string()));
    let _ = ClientEngine::new(cfg(None)).submit(bad, None, Instant::now());
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "a header the engine writes itself")]
fn a_spec_carrying_content_length_trips_the_debug_assert() {
    let mut bad = with_sdp(RtspMethod::Announce, b"v=0");
    bad.headers.push(("Content-Length", "3".to_string()));
    let _ = ClientEngine::new(cfg(None)).submit(bad, None, Instant::now());
}

// --- interim (1xx) replies ---

#[test]
fn a_100_then_a_200_completes_the_request_once() {
    for method in [
        RtspMethod::Announce,
        RtspMethod::Setup,
        RtspMethod::Record,
        RtspMethod::Teardown,
    ] {
        let t0 = Instant::now();
        let mut e = if method == RtspMethod::Record {
            ready(1)
        } else {
            ClientEngine::new(cfg(None))
        };
        let s = if method == RtspMethod::Announce {
            with_sdp(method, b"v=0\r\n")
        } else {
            spec(method, URI)
        };
        let a = e.submit(s, None, t0).unwrap();
        let c = cseq(&one(&mut e));
        e.handle_reply(
            resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            t0,
        );
        assert!(events(&mut e).is_empty(), "{method:?}: no reply to a 100");
        assert_eq!(e.counters().interim_replies, 1, "{method:?}");
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            t0,
        );
        let replies: Vec<_> = events(&mut e)
            .into_iter()
            .filter_map(|ev| match ev {
                Event::Reply { txn, result } => Some((txn, result)),
                _ => None,
            })
            .collect();
        assert_eq!(replies.len(), 1, "{method:?}: {replies:?}");
        assert_eq!(replies[0].0, a);
        assert_eq!(
            replies[0].1.as_ref().unwrap().response.status,
            200,
            "{method:?}"
        );
        assert_eq!(e.counters().unmatched_replies, 0, "{method:?}");
    }
}

#[test]
fn a_100_to_a_ping_keeps_the_ping_awaited() {
    let (mut e, t0, c) = pinged(None);
    e.handle_reply(
        resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().interim_replies, 1);
    assert_eq!(e.counters().keepalive_responses, 0);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().keepalive_responses, 1);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn a_100_alone_times_out_at_the_deadline() {
    let t0 = Instant::now();
    let mut e = ready(1);
    let a = e
        .submit(spec(RtspMethod::Record, URI), Some(ms(t0, 500)), t0)
        .unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        ms(t0, 100),
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.poll_timeout(), Some(ms(t0, 500)));
    e.handle_timeout(ms(t0, 499), false);
    assert!(events(&mut e).is_empty());
    e.handle_timeout(ms(t0, 500), false);
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
}

#[test]
fn a_flood_of_100s_does_not_extend_the_deadline() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let deadline = ms(t0, 2000);
    let a = e.submit(announce(1), Some(deadline), t0).unwrap();
    let c = cseq(&one(&mut e));
    for i in 0..1000 {
        e.handle_reply(
            resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            ms(t0, i),
        );
    }
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().interim_replies, 1000);
    assert_eq!(e.poll_timeout(), Some(deadline));
    e.handle_timeout(deadline, false);
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
}

#[test]
fn a_cseq_less_1xx_is_ignored_and_the_final_reply_completes() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Setup, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(resp(b"RTSP/1.0 100 Continue\r\n\r\n"), t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().interim_replies, 1);
    assert_eq!(e.counters().unmatched_replies, 0);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn a_1xx_with_an_unknown_cseq_is_unmatched() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    // Nothing waits: a 1xx with no CSeq answers nothing either.
    e.handle_reply(resp(b"RTSP/1.0 100 Continue\r\n\r\n"), t0);
    assert_eq!(e.counters().unmatched_replies, 1);
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(resp(b"RTSP/1.0 100 Continue\r\nCSeq: 7\r\n\r\n"), t0);
    e.handle_reply(resp(b"RTSP/1.0 199 Odd\r\nCSeq: 8\r\n\r\n"), t0);
    e.handle_reply(resp(b"RTSP/1.0 100 Continue\r\nCSeq: x\r\n\r\n"), t0);
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 4);
    assert_eq!(e.counters().interim_replies, 0);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

/// An OPTIONS answered `401` and retried under the next CSeq.
fn retried_options(t0: Instant) -> (ClientEngine, TxnId, u32) {
    let mut e = ClientEngine::new(cfg(Some(("user", "pass"))));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(
            format!(
                "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n"
            )
            .as_bytes(),
        ),
        t0,
    );
    let retry = cseq(&one(&mut e));
    assert_eq!(retry, c + 1);
    assert!(events(&mut e).is_empty());
    (e, a, retry)
}

#[test]
fn a_100_to_the_retry_after_a_401_is_ignored() {
    let t0 = Instant::now();
    let (mut e, a, retry) = retried_options(t0);
    e.handle_reply(
        resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {retry}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().interim_replies, 1);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {retry}\r\n\r\n").as_bytes()),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    assert_eq!(e.counters().unmatched_replies, 0);
}

/// Once the retry is out the first attempt's CSeq is waited on by
/// nothing: a `100` under it is unmatched, as its final reply would be.
#[test]
fn a_100_under_the_first_attempts_cseq_after_the_retry_is_unmatched_or_ignored_consistently() {
    let t0 = Instant::now();
    let (mut e, a, retry) = retried_options(t0);
    let first = retry - 1;
    e.handle_reply(
        resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {first}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 1);
    assert_eq!(e.counters().interim_replies, 0);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {first}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 2);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {retry}\r\n\r\n").as_bytes()),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
}

#[test]
fn a_100_after_the_final_reply_is_unmatched() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    e.handle_reply(
        resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().unmatched_replies, 1);
    assert_eq!(e.counters().interim_replies, 0);
}

// --- one CSeq counter ---

#[test]
fn pings_and_requests_share_one_counter() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    let a = e
        .submit(spec(RtspMethod::Describe, URI), None, ms(t0, 900))
        .unwrap();
    assert_eq!(cseq(&one(&mut e)), 1);
    e.handle_timeout(ms(t0, 1000), false);
    let p = one(&mut e);
    assert!(p.is_ping);
    assert_eq!(cseq(&p), 2);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n"), ms(t0, 1001));
    assert_eq!(reply(&mut e, a).unwrap().status, 200);
    let b = e
        .submit(spec(RtspMethod::Play, URI), None, ms(t0, 1002))
        .unwrap();
    assert_eq!(cseq(&one(&mut e)), 3);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n"), ms(t0, 1003));
    assert!(events(&mut e).is_empty());
    assert_eq!(e.counters().keepalive_responses, 1);
    e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nCSeq: 3\r\n\r\n"), ms(t0, 1004));
    assert_eq!(reply(&mut e, b).unwrap().status, 200);
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn the_cseq_counter_wraps_past_u32_max() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.set_next_cseq(u32::MAX);
    for want in ["4294967295", "0", "1"] {
        let a = e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
        let t = one(&mut e);
        assert_eq!(sent(&t).headers["cseq"], want);
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {want}\r\n\r\n").as_bytes()),
            t0,
        );
        assert_eq!(reply(&mut e, a).unwrap().status, 200, "CSeq {want}");
    }
    assert_eq!(e.counters().unmatched_replies, 0);
}

#[test]
fn a_reply_body_over_the_cap_to_announce_ends_the_stream_as_protocol_error() {
    let t0 = Instant::now();
    let mut e = interleaved();
    e.submit(with_sdp(RtspMethod::Announce, b"v=0\r\n"), None, t0)
        .unwrap();
    e.handle_input(
        format!(
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
            MAX_RTSP_BODY_BYTES + 1
        )
        .as_bytes(),
        t0,
    );
    let (msg, detail) = protocol_end(&events(&mut e));
    assert_eq!(
        msg,
        "malformed RTSP Content-Length: Content-Length exceeds maximum"
    );
    assert_eq!(detail, Some("Content-Length exceeds maximum"));
    assert_closed_by_protocol_error(&mut e, t0);
}

// --- the publish session ---

fn announce(tracks: u8) -> RequestSpec {
    RequestSpec {
        announce_tracks: Some(tracks),
        ..with_sdp(RtspMethod::Announce, b"v=0\r\n")
    }
}

fn record_setup(id: u8) -> RequestSpec {
    RequestSpec {
        setup: Some(push(id)),
        ..spec(RtspMethod::Setup, URI)
    }
}

/// The headers of an accepted SETUP for track `id`: its pair `2id-2id+1`
/// and session `P1`.
fn record_setup_headers(id: u8) -> String {
    format!(
        "Transport: RTP/AVP/TCP;unicast;interleaved={}-{};mode=record\r\nSession: P1\r\n",
        2 * id,
        2 * id + 1
    )
}

/// Submits `s` and answers it `status` with `headers` under its CSeq;
/// returns the reply's outcome.
fn exchange(
    e: &mut ClientEngine,
    s: RequestSpec,
    status: &str,
    headers: &str,
) -> Result<ReplyOutcome, RtspError> {
    let t0 = Instant::now();
    let a = e.submit(s, None, t0).unwrap();
    let c = cseq(&one(e));
    e.handle_input(
        format!("RTSP/1.0 {status}\r\nCSeq: {c}\r\n{headers}\r\n").as_bytes(),
        t0,
    );
    outcome(e, a)
}

fn announced(tracks: u8) -> ClientEngine {
    let mut e = ClientEngine::new(cfg(None));
    exchange(&mut e, announce(tracks), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
    e
}

fn set_up(e: &mut ClientEngine, id: u8) -> Result<ReplyOutcome, RtspError> {
    exchange(e, record_setup(id), "200 OK", &record_setup_headers(id))
}

fn ready(tracks: u8) -> ClientEngine {
    let mut e = announced(tracks);
    for id in 0..tracks {
        set_up(&mut e, id).unwrap();
    }
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
    e
}

fn recording() -> ClientEngine {
    let mut e = ready(1);
    exchange(
        &mut e,
        spec(RtspMethod::Record, URI),
        "200 OK",
        "Session: P1\r\n",
    )
    .unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    e
}

fn paused() -> ClientEngine {
    let mut e = recording();
    exchange(
        &mut e,
        spec(RtspMethod::Pause, URI),
        "200 OK",
        "Session: P1\r\n",
    )
    .unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Paused));
    e
}

/// `s` is refused before it is written: `NotAllowed`, nothing queued, the
/// phase unchanged, and the next request carries the next CSeq.
fn assert_refused(e: &mut ClientEngine, s: RequestSpec) {
    let next = e.cseq.wrapping_add(1);
    let phase = e.push_phase();
    let what = format!("{:?}", s.method);
    let r = e.submit(s, None, Instant::now());
    assert!(
        matches!(r, Err(RtspError::NotAllowed { .. })),
        "{what} in {phase:?}: {r:?}"
    );
    assert!(e.poll_transmit().is_none(), "{what} in {phase:?}");
    assert!(e.poll_event().is_none(), "{what} in {phase:?}");
    assert_eq!(e.push_phase(), phase, "{what}");
    e.submit(spec(RtspMethod::Options, URI), None, Instant::now())
        .unwrap();
    assert_eq!(cseq(&one(e)), next, "{what} in {phase:?}");
}

#[test]
fn announce_2xx_moves_to_announced() {
    for status in ["200 OK", "201 Created", "204 No Content"] {
        let t0 = Instant::now();
        let mut e = ClientEngine::new(cfg(None));
        let a = e.submit(announce(2), None, t0).unwrap();
        let c = cseq(&one(&mut e));
        assert_eq!(e.push_phase(), None, "{status}: moved before the reply");
        e.handle_reply(
            resp(format!("RTSP/1.0 {status}\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            t0,
        );
        assert!(outcome(&mut e, a).is_ok());
        assert_eq!(e.push_phase(), Some(PushPhase::Announced), "{status}");
        assert!(!e.media_allowed());
    }
}

#[test]
fn each_record_setup_2xx_advances_and_the_last_reaches_ready() {
    let mut e = announced(3);
    let o = set_up(&mut e, 0).unwrap();
    assert_eq!(o.setup, accepted(0, 0, 1));
    assert_eq!(e.push_phase(), Some(PushPhase::SetUp { done: 1, total: 3 }));
    let o = exchange(
        &mut e,
        record_setup(1),
        "201 Created",
        &record_setup_headers(1),
    )
    .unwrap();
    assert_eq!(o.setup, accepted(1, 2, 3));
    assert_eq!(e.push_phase(), Some(PushPhase::SetUp { done: 2, total: 3 }));
    let o = set_up(&mut e, 2).unwrap();
    assert_eq!(o.setup, accepted(2, 4, 5));
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
    assert!(!e.media_allowed());
    // One track: its SETUP reaches Ready at once.
    let mut e = announced(1);
    set_up(&mut e, 0).unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
}

#[test]
fn record_2xx_moves_to_recording() {
    for status in ["200 OK", "201 Created"] {
        let mut e = ready(2);
        let o = exchange(&mut e, spec(RtspMethod::Record, URI), status, "").unwrap();
        assert_eq!(o.setup, None);
        assert_eq!(e.push_phase(), Some(PushPhase::Recording), "{status}");
        assert!(e.media_allowed());
    }
}

#[test]
fn pause_2xx_moves_to_paused() {
    let mut e = recording();
    exchange(&mut e, spec(RtspMethod::Pause, URI), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Paused));
}

#[test]
fn record_in_paused_moves_back_to_recording() {
    let mut e = paused();
    exchange(&mut e, spec(RtspMethod::Record, URI), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    assert!(e.media_allowed());
}

/// A `1xx` and a refusal move nothing; the RECORD can be sent again.
#[test]
fn a_refused_record_leaves_ready_and_can_be_retried() {
    let t0 = Instant::now();
    let mut e = ready(1);
    let a = e.submit(spec(RtspMethod::Record, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 100 Continue\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(
        e.push_phase(),
        Some(PushPhase::Ready),
        "a 1xx moves nothing"
    );
    e.handle_reply(
        resp(
            format!("RTSP/1.0 455 Method Not Valid in This State\r\nCSeq: {c}\r\n\r\n").as_bytes(),
        ),
        t0,
    );
    assert_eq!(outcome(&mut e, a).unwrap().response.status, 455);
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
    assert!(!e.media_allowed());
    exchange(&mut e, spec(RtspMethod::Record, URI), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
}

#[test]
fn a_refused_setup_leaves_the_track_count() {
    let mut e = announced(2);
    set_up(&mut e, 0).unwrap();
    let o = exchange(
        &mut e,
        record_setup(1),
        "461 Unsupported Transport",
        &record_setup_headers(1),
    )
    .unwrap();
    assert_eq!((o.response.status, o.setup), (461, None));
    assert_eq!(e.push_phase(), Some(PushPhase::SetUp { done: 1, total: 2 }));
    assert!(rtcp_on(&mut e, 3).is_empty(), "a refusal installs nothing");
    let o = set_up(&mut e, 1).unwrap();
    assert_eq!(o.setup, accepted(1, 2, 3));
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
}

#[test]
fn announce_twice() {
    for mut e in every_live_phase() {
        assert_refused(&mut e, announce(1));
    }
}

#[test]
fn announce_on_an_rtsp_2_0_client() {
    let mut c = cfg(None);
    c.version = RtspVersion::V2_0;
    let mut e = ClientEngine::new(c);
    assert_refused(&mut e, announce(1));
    assert_eq!(e.push_phase(), None);
}

#[test]
fn an_announce_without_a_track_count_of_1_to_4_is_refused() {
    let mut e = ClientEngine::new(cfg(None));
    for tracks in [None, Some(0), Some(5), Some(u8::MAX)] {
        assert_refused(
            &mut e,
            RequestSpec {
                announce_tracks: tracks,
                ..announce(1)
            },
        );
    }
    exchange(&mut e, announce(4), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
}

#[test]
fn announce_after_a_pull_setup() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=0-1", t0).unwrap();
    assert_refused(&mut e, announce(1));
    // Still refused once the pull session is torn down.
    e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    events(&mut e);
    assert_refused(&mut e, announce(1));
    // A SETUP that named no session left the client free to publish.
    let mut e = ClientEngine::new(cfg(None));
    exchange(
        &mut e,
        spec(RtspMethod::Setup, URI),
        "461 Unsupported Transport",
        "",
    )
    .unwrap();
    exchange(&mut e, announce(1), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
}

#[test]
fn setup_record_before_announce() {
    let mut e = ClientEngine::new(cfg(None));
    assert_refused(&mut e, record_setup(0));
    assert_refused(&mut e, spec(RtspMethod::Record, URI));
    // Not while the ANNOUNCE waits for its reply either.
    e.submit(announce(1), None, Instant::now()).unwrap();
    one(&mut e);
    let next = e.cseq.wrapping_add(1);
    assert!(matches!(
        e.submit(record_setup(0), None, Instant::now()),
        Err(RtspError::NotAllowed { .. })
    ));
    assert!(e.poll_transmit().is_none());
    assert_eq!(e.cseq.wrapping_add(1), next);
}

#[test]
fn setup_record_out_of_track_order() {
    let mut e = announced(2);
    assert_refused(&mut e, record_setup(1));
    set_up(&mut e, 0).unwrap();
    assert_refused(&mut e, record_setup(0));
    assert_refused(&mut e, record_setup(2));
    set_up(&mut e, 1).unwrap();
    // Every announced track is set up.
    assert_refused(&mut e, record_setup(2));
    assert_refused(&mut e, record_setup(1));
    for mut e in [recording(), paused()] {
        assert_refused(&mut e, record_setup(0));
        assert_refused(&mut e, record_setup(1));
    }
}

#[test]
fn record_before_every_track_is_set_up() {
    let mut e = announced(2);
    assert_refused(&mut e, spec(RtspMethod::Record, URI));
    set_up(&mut e, 0).unwrap();
    assert_refused(&mut e, spec(RtspMethod::Record, URI));
    assert_eq!(e.push_phase(), Some(PushPhase::SetUp { done: 1, total: 2 }));
}

#[test]
fn record_while_recording() {
    let mut e = recording();
    assert_refused(&mut e, spec(RtspMethod::Record, URI));
    assert!(e.media_allowed());
}

#[test]
fn pause_before_recording() {
    let mut e = ClientEngine::new(cfg(None));
    exchange(&mut e, announce(2), "200 OK", "").unwrap();
    assert_refused(&mut e, spec(RtspMethod::Pause, URI));
    set_up(&mut e, 0).unwrap();
    assert_refused(&mut e, spec(RtspMethod::Pause, URI));
    set_up(&mut e, 1).unwrap();
    assert_refused(&mut e, spec(RtspMethod::Pause, URI));
    let mut e = paused();
    assert_refused(&mut e, spec(RtspMethod::Pause, URI));
}

/// Every phase from the ANNOUNCE's reply to the end, each on its own
/// engine.
fn every_live_phase() -> Vec<ClientEngine> {
    let mut partly = announced(2);
    set_up(&mut partly, 0).unwrap();
    vec![announced(1), partly, ready(1), recording(), paused()]
}

#[test]
fn describe_after_announce() {
    for mut e in every_live_phase() {
        assert_refused(&mut e, spec(RtspMethod::Describe, URI));
    }
}

#[test]
fn play_after_announce() {
    for mut e in every_live_phase() {
        assert_refused(&mut e, spec(RtspMethod::Play, URI));
    }
}

#[test]
fn a_pull_setup_after_announce() {
    for mut e in every_live_phase() {
        assert_refused(
            &mut e,
            RequestSpec {
                setup: Some(pull(0)),
                ..spec(RtspMethod::Setup, URI)
            },
        );
        assert_refused(&mut e, spec(RtspMethod::Setup, URI));
    }
}

#[test]
fn get_parameter_after_announce() {
    for mut e in every_live_phase() {
        assert_refused(&mut e, spec(RtspMethod::GetParameter, URI));
    }
}

/// OPTIONS changes no phase; TEARDOWN ends the session from any phase.
#[test]
fn options_and_teardown_are_accepted_in_every_live_phase() {
    for mut e in every_live_phase() {
        let phase = e.push_phase();
        let o = exchange(&mut e, spec(RtspMethod::Options, URI), "200 OK", "").unwrap();
        assert_eq!(o.response.status, 200);
        assert_eq!(e.push_phase(), phase);
        e.submit(spec(RtspMethod::Teardown, URI), None, Instant::now())
            .unwrap();
        assert_eq!(cseq(&one(&mut e)), e.cseq, "{phase:?}");
        assert_eq!(e.push_phase(), Some(PushPhase::Ended), "{phase:?}");
    }
}

#[test]
fn nothing_is_accepted_after_a_teardown_was_submitted() {
    let mut e = recording();
    let a = e
        .submit(spec(RtspMethod::Teardown, URI), None, Instant::now())
        .unwrap();
    let c = cseq(&one(&mut e));
    for s in [
        spec(RtspMethod::Options, URI),
        spec(RtspMethod::Record, URI),
        spec(RtspMethod::Pause, URI),
        spec(RtspMethod::Teardown, URI),
        announce(1),
        record_setup(1),
        spec(RtspMethod::Describe, URI),
        spec(RtspMethod::Play, URI),
        spec(RtspMethod::GetParameter, URI),
        spec(RtspMethod::Setup, URI),
        RequestSpec {
            setup: Some(pull(0)),
            ..spec(RtspMethod::Setup, URI)
        },
    ] {
        let what = format!("{:?}", s.method);
        let r = e.submit(s, None, Instant::now());
        assert!(
            matches!(r, Err(RtspError::NotAllowed { .. })),
            "{what}: {r:?}"
        );
        assert!(e.poll_transmit().is_none(), "{what}");
    }
    assert_eq!(e.cseq, c, "no number is taken");
    // The TEARDOWN is still the transaction waiting.
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        Instant::now(),
    );
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::Ended(EndReason::CleanTeardown), Event::Reply { txn, result: Ok(_) }]
                if *txn == a
        ),
        "{ev:?}"
    );
    assert_eq!(e.push_phase(), Some(PushPhase::Ended));
}

#[test]
fn record_201_with_location_is_accepted_and_location_ignored() {
    let mut e = ready(1);
    let o = exchange(
        &mut e,
        spec(RtspMethod::Record, URI),
        "201 Created",
        "Location: rtsp://elsewhere.example/other\r\n",
    )
    .unwrap();
    assert_eq!(o.response.status, 201);
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    assert!(e.poll_transmit().is_none(), "nothing goes to the Location");
    assert!(e.poll_event().is_none());
}

/// The engine hands the `3xx` to the caller as the reply (the facade
/// reports it as `Protocol`) and writes nothing to the new location.
#[test]
fn a_3xx_to_announce_is_returned_unfollowed_and_moves_no_phase() {
    for status in ["301 Moved Permanently", "302 Moved Temporarily"] {
        let mut e = ClientEngine::new(cfg(None));
        let o = exchange(
            &mut e,
            announce(1),
            status,
            "Location: rtsp://elsewhere.example/live\r\n",
        )
        .unwrap();
        assert!((300..400).contains(&o.response.status), "{status}");
        assert_eq!(e.push_phase(), None, "{status}");
        assert!(e.poll_transmit().is_none(), "{status}: not followed");
        assert!(e.poll_event().is_none());
    }
}

#[test]
fn a_second_setup_reply_naming_another_session_is_refused_and_the_first_id_kept() {
    let t0 = Instant::now();
    let mut e = announced(2);
    // No override: a learned timeout would retune the interval.
    e.enable_keepalive(None, t0);
    set_up(&mut e, 0).unwrap();
    let r = exchange(
        &mut e,
        record_setup(1),
        "200 OK",
        "Transport: RTP/AVP/TCP;unicast;interleaved=2-3;mode=record\r\nSession: P2;timeout=4\r\n",
    );
    assert!(matches!(r, Err(RtspError::BadResponse { .. })), "{r:?}");
    assert_eq!(e.push_phase(), Some(PushPhase::SetUp { done: 1, total: 2 }));
    let before = e.counters().malformed_frames;
    assert!(rtcp_on(&mut e, 3).is_empty(), "no route is installed");
    assert_eq!(e.counters().malformed_frames, before + 1);
    // Neither the id nor its timeout was learned: the interval is still
    // half the default 60 s, not half of 4 s.
    assert_eq!(e.poll_timeout(), Some(ms(t0, 30_000)));
    e.handle_timeout(ms(t0, 30_000), false);
    assert_eq!(sent(&one(&mut e)).headers["session"], "P1");
    // The same id, with or without a timeout, is the same session.
    let o = exchange(
        &mut e,
        record_setup(1),
        "200 OK",
        "Transport: RTP/AVP/TCP;unicast;interleaved=2-3;mode=record\r\nSession: P1;timeout=60\r\n",
    )
    .unwrap();
    assert_eq!(o.setup, accepted(1, 2, 3));
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
}

/// A pull SETUP reply the pull facade refuses (any `2xx` but `200`)
/// changes nothing: not the id, not the ping interval, not whether the
/// client may publish.
#[test]
fn a_pull_setup_201_changes_no_session_state() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(None, t0);
    setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=0-1", t0).unwrap();
    let o = exchange(
        &mut e,
        RequestSpec {
            setup: Some(pull(0)),
            ..spec(RtspMethod::Setup, URI)
        },
        "201 Created",
        "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\nSession: S2;timeout=4\r\n",
    )
    .unwrap();
    assert_eq!((o.response.status, o.setup), (201, None));
    assert_eq!(e.poll_timeout(), Some(ms(t0, 30_000)), "not retuned to 2 s");
    e.handle_timeout(ms(t0, 30_000), false);
    assert_eq!(sent(&one(&mut e)).headers["session"], "S1");
    // A client whose only SETUP was such a reply may still publish.
    let mut e = ClientEngine::new(cfg(None));
    exchange(
        &mut e,
        spec(RtspMethod::Setup, URI),
        "201 Created",
        "Transport: RTP/AVP;unicast;client_port=5004-5005\r\nSession: S2\r\n",
    )
    .unwrap();
    assert!(e.session_id.is_none());
    exchange(&mut e, announce(1), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
}

/// A push SETUP the client refuses for its channel pair pins no session:
/// the retry, which a server answers with a new session, is taken.
#[test]
fn a_setup_the_install_refused_does_not_pin_its_session_id() {
    for (pair, held) in [("0-0", None), ("6-7", Some((6, 7)))] {
        let t0 = Instant::now();
        let mut e = announced(2);
        e.enable_keepalive(None, t0);
        if let Some((rtp, rtcp)) = held {
            // A pair another stream holds.
            e.install_route(pull(3), rtp, rtcp).unwrap();
        }
        let r = exchange(
            &mut e,
            record_setup(0),
            "200 OK",
            &format!(
                "Transport: RTP/AVP/TCP;unicast;interleaved={pair};mode=record\r\nSession: X;timeout=4\r\n"
            ),
        );
        assert!(
            matches!(r, Err(RtspError::BadResponse { .. })),
            "{pair}: {r:?}"
        );
        assert_eq!(e.push_phase(), Some(PushPhase::Announced), "{pair}");
        assert!(e.session_id.is_none(), "{pair}");
        assert_eq!(e.poll_timeout(), Some(ms(t0, 30_000)), "{pair}");
        let o = exchange(
            &mut e,
            record_setup(0),
            "200 OK",
            "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\nSession: Y\r\n",
        )
        .unwrap();
        assert_eq!(o.setup, accepted(0, 0, 1), "{pair}");
        assert_eq!(e.push_phase(), Some(PushPhase::SetUp { done: 1, total: 2 }));
        e.handle_timeout(ms(t0, 30_000), false);
        assert_eq!(sent(&one(&mut e)).headers["session"], "Y", "{pair}");
    }
}

/// A publish session sends its media interleaved: a SETUP reply that
/// names another transport is refused and the track stays to be set up.
#[test]
fn a_push_setup_2xx_that_is_not_interleaved_is_bad_response_and_the_phase_stays() {
    let mut e = announced(1);
    let r = exchange(
        &mut e,
        record_setup(0),
        "200 OK",
        "Transport: RTP/AVP;unicast;client_port=5004-5005;server_port=6000-6001\r\nSession: U\r\n",
    );
    assert!(matches!(r, Err(RtspError::BadResponse { .. })), "{r:?}");
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
    assert!(e.session_id.is_none());
    assert!(!e.media_allowed());
    let o = set_up(&mut e, 0).unwrap();
    assert_eq!(o.setup, accepted(0, 0, 1));
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
}

/// The pull side still takes the id of the latest SETUP reply.
#[test]
fn a_second_pull_setup_reply_naming_another_session_replaces_it() {
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    run_setup(&mut e, "S1", t0);
    run_setup(&mut e, "S2", t0);
    e.handle_timeout(ms(t0, 1000), false);
    assert_eq!(sent(&one(&mut e)).headers["session"], "S2");
}

#[test]
fn media_is_allowed_only_in_recording_and_paused() {
    let mut e = ClientEngine::new(cfg(None));
    assert!(!e.media_allowed());
    exchange(&mut e, announce(2), "200 OK", "").unwrap();
    assert!(!e.media_allowed());
    set_up(&mut e, 0).unwrap();
    assert!(!e.media_allowed());
    set_up(&mut e, 1).unwrap();
    assert!(!e.media_allowed());
    exchange(&mut e, spec(RtspMethod::Record, URI), "200 OK", "").unwrap();
    assert!(e.media_allowed());
    exchange(&mut e, spec(RtspMethod::Pause, URI), "200 OK", "").unwrap();
    assert!(e.media_allowed());
    exchange(&mut e, spec(RtspMethod::Record, URI), "200 OK", "").unwrap();
    assert!(e.media_allowed());
    // A pull client never sends media.
    let t0 = Instant::now();
    let mut e = ClientEngine::new(cfg(None));
    setup_route(&mut e, pull(0), "RTP/AVP/TCP;unicast;interleaved=0-1", t0).unwrap();
    exchange(&mut e, spec(RtspMethod::Play, URI), "200 OK", "").unwrap();
    assert!(!e.media_allowed());
}

#[test]
fn teardown_submitted_ends_media_at_once() {
    for mut e in [recording(), paused()] {
        e.submit(spec(RtspMethod::Teardown, URI), None, Instant::now())
            .unwrap();
        assert!(!e.media_allowed(), "before the TEARDOWN is even written");
        assert_eq!(e.push_phase(), Some(PushPhase::Ended));
        let c = cseq(&one(&mut e));
        // Not even a refusal brings it back.
        e.handle_reply(
            resp(format!("RTSP/1.0 500 Internal Server Error\r\nCSeq: {c}\r\n\r\n").as_bytes()),
            Instant::now(),
        );
        events(&mut e);
        assert!(!e.media_allowed());
        assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    }
}

#[test]
fn a_record_reply_after_teardown_was_submitted_does_not_reopen_media() {
    for mut e in [ready(1), paused()] {
        let t0 = Instant::now();
        e.submit(spec(RtspMethod::Record, URI), None, t0).unwrap();
        let record = cseq(&one(&mut e));
        let td = e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
        one(&mut e);
        e.handle_reply(
            resp(format!("RTSP/1.0 200 OK\r\nCSeq: {record}\r\n\r\n").as_bytes()),
            t0,
        );
        assert!(events(&mut e).is_empty());
        assert_eq!(e.counters().unmatched_replies, 1);
        assert!(!e.media_allowed());
        assert_eq!(e.push_phase(), Some(PushPhase::Ended));
        // A RECORD reply without a CSeq is taken as the TEARDOWN's.
        e.handle_reply(resp(b"RTSP/1.0 200 OK\r\nSession: P1\r\n\r\n"), t0);
        let ev = events(&mut e);
        assert!(
            ev.iter()
                .any(|ev| matches!(ev, Event::Reply { txn, .. } if *txn == td)),
            "{ev:?}"
        );
        assert!(!e.media_allowed());
        assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    }
}

/// A reply for a request the caller replaced moves nothing.
#[test]
fn a_reply_to_a_replaced_record_moves_nothing() {
    let t0 = Instant::now();
    let mut e = ready(1);
    e.submit(spec(RtspMethod::Record, URI), None, t0).unwrap();
    let record = cseq(&one(&mut e));
    e.submit(spec(RtspMethod::Options, URI), None, t0).unwrap();
    one(&mut e);
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {record}\r\n\r\n").as_bytes()),
        t0,
    );
    assert!(events(&mut e).is_empty());
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
    assert!(!e.media_allowed());
}

/// A record reply past its deadline is a timeout and moves nothing.
#[test]
fn a_record_reply_after_its_deadline_moves_nothing() {
    let t0 = Instant::now();
    let mut e = ready(1);
    let a = e
        .submit(spec(RtspMethod::Record, URI), Some(ms(t0, 500)), t0)
        .unwrap();
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 200 OK\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        ms(t0, 500),
    );
    assert!(matches!(reply(&mut e, a), Err(RtspError::Timeout)));
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
}

/// A ping answered `454` ends the session: media stops, and only the
/// TEARDOWN that closes the client is still written.
#[test]
fn a_ping_454_ends_media() {
    let t0 = Instant::now();
    let mut e = recording();
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    e.handle_timeout(ms(t0, 1000), false);
    let c = cseq(&one(&mut e));
    e.handle_reply(
        resp(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {c}\r\n\r\n").as_bytes()),
        t0,
    );
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::SessionDead, Event::Ended(EndReason::SessionExpired)]
        ),
        "{ev:?}"
    );
    assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    assert!(!e.media_allowed());
    for s in [
        spec(RtspMethod::Record, URI),
        spec(RtspMethod::Pause, URI),
        spec(RtspMethod::Options, URI),
    ] {
        let r = e.submit(s, None, t0);
        assert!(matches!(r, Err(RtspError::NotAllowed { .. })), "{r:?}");
        assert!(e.poll_transmit().is_none());
    }
    e.submit(spec(RtspMethod::Teardown, URI), None, t0).unwrap();
    assert_eq!(cseq(&one(&mut e)), c + 1);
    assert!(matches!(
        e.submit(spec(RtspMethod::Teardown, URI), None, t0),
        Err(RtspError::NotAllowed { .. })
    ));
}

/// A ping that cannot be encoded ends the session as a `454` does.
#[test]
fn a_ping_refused_at_encoding_ends_media() {
    let t0 = Instant::now();
    let mut e = announced(1);
    e.enable_keepalive(Some(Duration::from_secs(1)), t0);
    exchange(
        &mut e,
        record_setup(0),
        "200 OK",
        "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\nSession: P\x01\r\n",
    )
    .unwrap();
    exchange(&mut e, spec(RtspMethod::Record, URI), "200 OK", "").unwrap();
    assert!(e.media_allowed());
    e.handle_timeout(ms(t0, 1000), false);
    assert!(e.poll_transmit().is_none());
    events(&mut e);
    assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    assert!(!e.media_allowed());
}

#[test]
fn a_stream_end_moves_to_ended() {
    let mut e = recording();
    e.handle_closed(StreamEnd::PeerClosed);
    events(&mut e);
    assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    assert!(!e.media_allowed());
    assert!(matches!(
        e.submit(spec(RtspMethod::Teardown, URI), None, Instant::now()),
        Err(RtspError::Io(io::ErrorKind::UnexpectedEof))
    ));
    // A framing violation ends it too.
    let t0 = Instant::now();
    let mut e = paused();
    e.handle_input(
        b"RTSP/1.0 200 OK\r\nCSeq: 9\r\nContent-Length: x\r\n\r\n",
        t0,
    );
    events(&mut e);
    assert!(e.is_closed());
    assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    assert!(!e.media_allowed());
    // So does a stop.
    let mut e = recording();
    e.handle_closed(StreamEnd::Stopped);
    assert_eq!(e.push_phase(), Some(PushPhase::Ended));
    assert!(!e.media_allowed());
}

#[test]
fn a_server_request_during_record_is_counted_and_answers_nothing() {
    let t0 = Instant::now();
    let mut e = recording();
    let before = (e.counters().unmatched_replies, e.counters().rtsp_messages);
    let mut raw = b"GET_PARAMETER rtsp://cam.example/live RTSP/1.0\r\nCSeq: 5\r\n\r\n".to_vec();
    raw.extend_from_slice(b"OPTIONS * RTSP/1.0\r\nCSeq: 6\r\n\r\n");
    raw.extend_from_slice(b"TEARDOWN rtsp://cam.example/live RTSP/1.0\r\nCSeq: 7\r\n\r\n");
    e.handle_input(&raw, t0);
    assert!(e.poll_event().is_none());
    assert!(e.poll_transmit().is_none(), "nothing is answered");
    assert_eq!(e.counters().unmatched_replies, before.0 + 3);
    assert_eq!(e.counters().rtsp_messages, before.1 + 3);
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    assert!(e.media_allowed());
}

#[test]
fn a_zero_or_huge_session_timeout_keeps_the_ping_floor_on_a_push() {
    for enable_first in [true, false] {
        let t0 = Instant::now();
        let mut e = announced(1);
        if enable_first {
            e.enable_keepalive(None, t0);
        }
        exchange(
            &mut e,
            record_setup(0),
            "200 OK",
            "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\nSession: P1;timeout=0\r\n",
        )
        .unwrap();
        if !enable_first {
            e.enable_keepalive(None, t0);
        }
        assert_eq!(
            e.poll_timeout(),
            Some(ms(t0, 100)),
            "enable first: {enable_first}"
        );

        let mut e = announced(1);
        if enable_first {
            e.enable_keepalive(None, t0);
        }
        exchange(
            &mut e,
            record_setup(0),
            "200 OK",
            &format!(
                "Transport: RTP/AVP/TCP;unicast;interleaved=0-1;mode=record\r\nSession: P1;timeout={}\r\n",
                u64::MAX
            ),
        )
        .unwrap();
        if !enable_first {
            e.enable_keepalive(None, t0);
        }
        let far = t0 + Duration::from_secs(100 * 365 * 24 * 3600);
        assert!(
            e.poll_timeout().is_none_or(|d| d > far),
            "enable first: {enable_first}"
        );
        e.handle_timeout(t0 + Duration::from_secs(3600), false);
        assert!(e.poll_transmit().is_none());
    }
}

#[test]
fn a_refused_pause_leaves_recording() {
    let mut e = recording();
    let o = exchange(
        &mut e,
        spec(RtspMethod::Pause, URI),
        "455 Method Not Valid in This State",
        "",
    )
    .unwrap();
    assert_eq!(o.response.status, 455);
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    assert!(e.media_allowed());
    exchange(&mut e, spec(RtspMethod::Pause, URI), "200 OK", "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Paused));
}

/// Submits `s`, answers it `401` and its retry `200` with `headers`; the
/// phase moves once, at the retry's reply.
fn exchange_after_a_401(
    e: &mut ClientEngine,
    s: RequestSpec,
    headers: &str,
) -> Result<ReplyOutcome, RtspError> {
    let t0 = Instant::now();
    let before = e.push_phase();
    let a = e.submit(s, None, t0).unwrap();
    let c = cseq(&one(e));
    e.handle_input(
        format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: {c}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\n\r\n"
        )
        .as_bytes(),
        t0,
    );
    assert!(events(e).is_empty());
    assert_eq!(e.push_phase(), before, "a 401 moves nothing");
    let retry = cseq(&one(e));
    assert_eq!(retry, c + 1);
    e.handle_input(
        format!("RTSP/1.0 200 OK\r\nCSeq: {retry}\r\n{headers}\r\n").as_bytes(),
        t0,
    );
    outcome(e, a)
}

#[test]
fn a_401_retry_moves_the_phase_once_at_the_final_reply() {
    let mut e = ClientEngine::new(cfg(Some(("user", "pw"))));
    exchange_after_a_401(&mut e, announce(1), "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
    let o = exchange_after_a_401(&mut e, record_setup(0), &record_setup_headers(0)).unwrap();
    assert_eq!(o.setup, accepted(0, 0, 1));
    assert_eq!(e.push_phase(), Some(PushPhase::Ready));
    exchange_after_a_401(&mut e, spec(RtspMethod::Record, URI), "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    exchange_after_a_401(&mut e, spec(RtspMethod::Pause, URI), "").unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Paused));
}

#[test]
fn a_record_setup_reply_after_its_deadline_moves_nothing() {
    let t0 = Instant::now();
    let mut e = announced(1);
    let a = e.submit(record_setup(0), Some(ms(t0, 500)), t0).unwrap();
    let c = cseq(&one(&mut e));
    let mut raw = format!(
        "RTSP/1.0 200 OK\r\nCSeq: {c}\r\n{}\r\n",
        record_setup_headers(0)
    )
    .into_bytes();
    raw.extend(interleaved_frame(1, b"\xDE\xAD\xBE\xEF"));
    e.handle_input(&raw, ms(t0, 500));
    let ev = events(&mut e);
    assert!(
        matches!(
            ev.as_slice(),
            [Event::Reply { txn, result: Err(RtspError::Timeout) }] if *txn == a
        ),
        "{ev:?}"
    );
    assert_eq!(e.push_phase(), Some(PushPhase::Announced));
    assert!(e.session_id.is_none());
    assert_eq!(e.counters().malformed_frames, 1, "no route was installed");
}

/// Only a SETUP reply teaches the session: a `Session` naming another id on
/// a RECORD or PAUSE reply is not followed.
#[test]
fn a_session_named_by_a_record_or_pause_reply_is_ignored() {
    let t0 = Instant::now();
    let mut e = ready(1);
    e.enable_keepalive(None, t0);
    exchange(
        &mut e,
        spec(RtspMethod::Record, URI),
        "200 OK",
        "Session: Q1;timeout=4\r\n",
    )
    .unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Recording));
    exchange(
        &mut e,
        spec(RtspMethod::Pause, URI),
        "200 OK",
        "Session: Q2;timeout=4\r\n",
    )
    .unwrap();
    assert_eq!(e.push_phase(), Some(PushPhase::Paused));
    assert_eq!(e.poll_timeout(), Some(ms(t0, 30_000)));
    e.handle_timeout(ms(t0, 30_000), false);
    assert_eq!(sent(&one(&mut e)).headers["session"], "P1");
}
