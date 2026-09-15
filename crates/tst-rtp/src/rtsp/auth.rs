//! Basic (RFC 7617) and Digest (RFC 7616 MD5 + SHA-256 + RFC 2617-flavored)
//! authentication for the RTSP client.
//!
//! The client never sends credentials preemptively; auth is always
//! driven by a 401 Unauthorized response with `WWW-Authenticate` header.

use secrecy::{ExposeSecret, SecretString};

use crate::error::RtspError;
use crate::rtsp::message::RtspMethod;

/// An auth challenge parsed from a 401 `WWW-Authenticate` header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthChallenge {
    Basic { realm: String },
    Digest(DigestChallenge),
}

/// A parsed `Digest` challenge from `WWW-Authenticate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    pub algorithm: DigestAlgorithm,
    pub qop: Option<Vec<String>>, // ["auth"], ["auth-int"], or both
    pub stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DigestAlgorithm {
    /// RFC 2617 default; also RFC 7616 §3.4 with `algorithm=MD5`.
    Md5,
    /// RFC 7616 §3.4 with `algorithm=SHA-256`.
    Sha256,
    /// RFC 7616 §3.4.2 session variant — adds cnonce to A1 hash.
    Md5Sess,
    Sha256Sess,
}

/// Parse one or more challenges from a comma-separated `WWW-Authenticate`
/// header. Per RFC 7235, a single header may contain multiple challenges;
/// servers commonly send only one but cameras occasionally send both
/// Basic and Digest, in which case we prefer Digest.
pub fn parse_challenges(www_authenticate: &str) -> Vec<AuthChallenge> {
    // Naive split-on-comma is wrong because Digest values contain commas
    // inside quoted strings ("realm="Foo, Bar"" is one challenge).
    // We tokenize scheme-by-scheme.
    let mut out = Vec::new();
    let bytes = www_authenticate.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Skip whitespace + commas
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // Scheme is the first token up to whitespace
        let scheme_start = i;
        while i < bytes.len() && bytes[i] != b' ' {
            i += 1;
        }
        let scheme = www_authenticate[scheme_start..i].to_ascii_lowercase();
        // Find the end of this challenge — next "<word>," that starts at
        // the top level (not inside a quoted string). For simplicity:
        // collect parameters one at a time.
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        let mut params: Vec<(String, String)> = Vec::new();
        loop {
            if i >= bytes.len() {
                break;
            }
            // Parse key=value
            let key_start = i;
            while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] == b',' {
                // Bare token: end of this challenge or beginning of next scheme
                let token = &www_authenticate[key_start..i];
                let trimmed = token.trim();
                if !trimmed.is_empty() && !trimmed.contains('=') {
                    // Could be the next scheme — re-parse from key_start
                    i = key_start;
                    break;
                }
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            let key_raw = www_authenticate[key_start..i].trim();
            // `Basic realm` / `Digest realm`: an auth-param name is a single
            // token (RFC 7235 §2.1), so whitespace inside the would-be key
            // means the previous challenge ended and a new `<scheme> <params>`
            // starts at `key_start` — hand it back to the outer loop. Without
            // this a joined `Digest …, Basic realm="r"` swallowed the second
            // scheme into the first challenge's parameter list, and
            // `Basic …, Digest …` kept the Digest→Basic downgrade CORR-05
            // exists to close.
            if key_raw.contains(char::is_whitespace) {
                i = key_start;
                break;
            }
            let key = key_raw.to_string();
            i += 1; // skip '='
            // Value: either quoted or token until comma
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
                let val_start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    // Skip escaped quotes (RFC 7616 §3.4 quoted-string)
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                    }
                    i += 1;
                }
                let val = www_authenticate[val_start..i].to_string();
                if i < bytes.len() {
                    i += 1;
                }
                params.push((key.to_ascii_lowercase(), val));
            } else {
                let val_start = i;
                while i < bytes.len() && bytes[i] != b',' && bytes[i] != b' ' {
                    i += 1;
                }
                let val = www_authenticate[val_start..i].to_string();
                params.push((key.to_ascii_lowercase(), val));
            }
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b',' {
                i += 1;
            }
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
        }
        // Build the challenge from params
        match scheme.as_str() {
            "basic" => {
                let realm = params
                    .iter()
                    .find(|(k, _)| k == "realm")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                out.push(AuthChallenge::Basic { realm });
            }
            "digest" => {
                let realm = params
                    .iter()
                    .find(|(k, _)| k == "realm")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let nonce = params
                    .iter()
                    .find(|(k, _)| k == "nonce")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let opaque = params
                    .iter()
                    .find(|(k, _)| k == "opaque")
                    .map(|(_, v)| v.clone());
                let algorithm = match params
                    .iter()
                    .find(|(k, _)| k == "algorithm")
                    .map(|(_, v)| v.as_str())
                {
                    None | Some("MD5") | Some("md5") => DigestAlgorithm::Md5,
                    Some("SHA-256") | Some("sha-256") => DigestAlgorithm::Sha256,
                    Some("MD5-sess") | Some("md5-sess") => DigestAlgorithm::Md5Sess,
                    Some("SHA-256-sess") | Some("sha-256-sess") => DigestAlgorithm::Sha256Sess,
                    Some(_other) => {
                        // Unknown algorithm — skip this challenge.
                        continue;
                    }
                };
                let qop = params
                    .iter()
                    .find(|(k, _)| k == "qop")
                    .map(|(_, v)| v.split(',').map(|s| s.trim().to_string()).collect());
                let stale = params
                    .iter()
                    .any(|(k, v)| k == "stale" && v.eq_ignore_ascii_case("true"));
                out.push(AuthChallenge::Digest(DigestChallenge {
                    realm,
                    nonce,
                    opaque,
                    algorithm,
                    qop,
                    stale,
                }));
            }
            _ => { /* unknown scheme — skip */ }
        }
    }
    out
}

/// Build the `Authorization:` header value for Basic auth (RFC 7617).
pub fn build_basic_response(username: &str, password: &SecretString) -> String {
    use base64::Engine;
    let creds = format!("{}:{}", username, password.expose_secret());
    let encoded = base64::engine::general_purpose::STANDARD.encode(creds.as_bytes());
    format!("Basic {}", encoded)
}

/// Parameters needed to compute a Digest response.
pub struct DigestContext<'a> {
    pub username: &'a str,
    pub password: &'a SecretString,
    pub method: &'a str, // "OPTIONS", "DESCRIBE", etc.
    pub uri: &'a str,    // The Request-URI we're sending
    pub nc: u32,         // Nonce-count, starts at 1
    pub cnonce: &'a str, // Client nonce, random per request
    pub challenge: &'a DigestChallenge,
}

/// Build the `Authorization:` header value for Digest auth per
/// RFC 7616 §3.4. Handles both `qop=auth` (with nc + cnonce) and the
/// older RFC 2617 no-qop variant when `challenge.qop` is None.
pub fn build_digest_response(ctx: &DigestContext<'_>) -> String {
    use super::digest::{self, Algo};

    let algo = match ctx.challenge.algorithm {
        DigestAlgorithm::Md5 | DigestAlgorithm::Md5Sess => Algo::Md5,
        DigestAlgorithm::Sha256 | DigestAlgorithm::Sha256Sess => Algo::Sha256,
    };

    // A1 per RFC 7616 §3.4.2; sess variants further hash with nonce+cnonce.
    let ha1_str = if matches!(
        ctx.challenge.algorithm,
        DigestAlgorithm::Md5Sess | DigestAlgorithm::Sha256Sess
    ) {
        let base = digest::ha1(algo, ctx.username, &ctx.challenge.realm, ctx.password);
        digest::hash(
            algo,
            &format!("{}:{}:{}", base, ctx.challenge.nonce, ctx.cnonce),
        )
    } else {
        digest::ha1(algo, ctx.username, &ctx.challenge.realm, ctx.password)
    };

    let qop_chosen = ctx
        .challenge
        .qop
        .as_ref()
        .and_then(|q| q.iter().find(|s| s.as_str() == "auth"))
        .cloned();

    let (nc_str, qop_str): (String, &str) = match qop_chosen.as_deref() {
        Some(qop) => (format!("{:08x}", ctx.nc), qop),
        None => (String::new(), ""),
    };

    let resp = digest::response(
        algo,
        &ha1_str,
        ctx.method,
        ctx.uri,
        &ctx.challenge.nonce,
        &nc_str,
        ctx.cnonce,
        qop_str,
    );

    // Build the Authorization header value.
    let algorithm_str = match ctx.challenge.algorithm {
        DigestAlgorithm::Md5 => "MD5",
        DigestAlgorithm::Sha256 => "SHA-256",
        DigestAlgorithm::Md5Sess => "MD5-sess",
        DigestAlgorithm::Sha256Sess => "SHA-256-sess",
    };
    let mut out = format!(
        r#"Digest username="{}", realm="{}", nonce="{}", uri="{}", response="{}", algorithm={}"#,
        ctx.username, ctx.challenge.realm, ctx.challenge.nonce, ctx.uri, resp, algorithm_str,
    );
    if let Some(qop) = &qop_chosen {
        out.push_str(&format!(
            r#", qop={}, nc={:08x}, cnonce="{}""#,
            qop, ctx.nc, ctx.cnonce
        ));
    }
    if let Some(opaque) = &ctx.challenge.opaque {
        out.push_str(&format!(r#", opaque="{}""#, opaque));
    }
    out
}

/// ASCII method token used in the Digest HA2 (`method:uri`).
pub(crate) fn method_token(method: RtspMethod) -> &'static str {
    match method {
        RtspMethod::Options => "OPTIONS",
        RtspMethod::Describe => "DESCRIBE",
        RtspMethod::Setup => "SETUP",
        RtspMethod::Play => "PLAY",
        RtspMethod::Pause => "PAUSE",
        RtspMethod::Teardown => "TEARDOWN",
        RtspMethod::GetParameter => "GET_PARAMETER",
    }
}

/// Build an `Authorization:` header value for `method` at `uri` from a raw
/// `WWW-Authenticate` challenge string and credentials. Prefers Digest over
/// Basic when both are offered.
///
/// `uri` MUST be the exact request-URI of the target method — gortsplib /
/// MediaMTX hash the Digest HA2 against the request URI, so SETUP must pass
/// its control URI (`…/trackID=0`), not the base URL.
///
/// `nc` is the nonce-count for the `qop=auth` form (RFC 7616 §3.4); it is
/// ignored by the RFC 2617 no-qop variant. Callers reusing one cached nonce
/// across requests MUST pass a strictly increasing `nc` so a `qop=auth`
/// server does not reject later requests as replays.
///
/// # Errors
///
/// - [`RtspError::AuthUnsupported`] if no recognized scheme is present.
/// - [`RtspError::Io`] if the OS RNG fails while generating the cnonce.
pub(crate) fn build_authorization(
    method: RtspMethod,
    uri: &str,
    www_auth: &str,
    username: &str,
    password: &SecretString,
    nc: u32,
) -> Result<String, RtspError> {
    let challenges = parse_challenges(www_auth);
    // Prefer Digest over Basic when both are offered.
    let challenge = challenges
        .iter()
        .find(|c| matches!(c, AuthChallenge::Digest(_)))
        .or_else(|| {
            challenges
                .iter()
                .find(|c| matches!(c, AuthChallenge::Basic { .. }))
        })
        .ok_or_else(|| RtspError::AuthUnsupported {
            scheme: "(no recognized scheme in WWW-Authenticate)".into(),
        })?;

    Ok(match challenge {
        AuthChallenge::Basic { .. } => build_basic_response(username, password),
        AuthChallenge::Digest(d) => {
            // Random cnonce — used only by the qop=auth path; ignored by the
            // RFC 2617 no-qop variant gortsplib/MediaMTX send.
            let mut cnonce_bytes = [0u8; 16];
            getrandom::getrandom(&mut cnonce_bytes)
                .map_err(|_| RtspError::Io(std::io::ErrorKind::Other))?;
            let mut cnonce = String::with_capacity(32);
            for b in &cnonce_bytes {
                use std::fmt::Write as _;
                let _ = write!(cnonce, "{b:02x}");
            }
            let ctx = DigestContext {
                username,
                password,
                method: method_token(method),
                uri,
                nc,
                cnonce: &cnonce,
                challenge: d,
            };
            build_digest_response(&ctx)
        }
    })
}

#[cfg(test)]
mod basic_tests {
    use super::*;

    #[test]
    fn parse_basic_challenge() {
        let h = r#"Basic realm="My Camera""#;
        let challenges = parse_challenges(h);
        assert_eq!(challenges.len(), 1);
        assert_eq!(
            challenges[0],
            AuthChallenge::Basic {
                realm: "My Camera".to_string()
            }
        );
    }

    #[test]
    fn build_basic_response_matches_rfc7617_example() {
        // RFC 7617 §2 example: Aladdin:open sesame → "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        let pw = SecretString::new("open sesame".into());
        let header = build_basic_response("Aladdin", &pw);
        assert_eq!(header, "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn parse_basic_without_realm() {
        let h = "Basic";
        let challenges = parse_challenges(h);
        assert_eq!(challenges.len(), 1);
        assert_eq!(
            challenges[0],
            AuthChallenge::Basic {
                realm: String::new()
            }
        );
    }

    /// CORR-05: a joined multi-challenge header (what the response parser
    /// now produces from two `WWW-Authenticate` lines) must split into BOTH
    /// challenges in either order. An auth-param name is a single token
    /// (RFC 7235 §2.1), so whitespace inside a would-be key marks the start
    /// of the next `<scheme> <params>`.
    #[test]
    fn parse_challenges_splits_basic_then_digest() {
        let h = r#"Basic realm="r", Digest realm="r", nonce="n""#;
        let challenges = parse_challenges(h);
        assert_eq!(challenges.len(), 2, "got {challenges:?}");
        assert_eq!(
            challenges[0],
            AuthChallenge::Basic {
                realm: "r".to_string()
            }
        );
        match &challenges[1] {
            AuthChallenge::Digest(d) => {
                assert_eq!(d.realm, "r");
                assert_eq!(d.nonce, "n");
            }
            other => panic!("expected Digest, got {other:?}"),
        }
    }

    #[test]
    fn parse_challenges_splits_digest_then_basic() {
        let h = r#"Digest realm="r", nonce="n", Basic realm="r""#;
        let challenges = parse_challenges(h);
        assert_eq!(challenges.len(), 2, "got {challenges:?}");
        assert!(matches!(challenges[0], AuthChallenge::Digest(_)));
        assert_eq!(
            challenges[1],
            AuthChallenge::Basic {
                realm: "r".to_string()
            }
        );
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn parse_md5_digest_challenge() {
        let h = r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
        let challenges = parse_challenges(h);
        assert_eq!(challenges.len(), 1);
        match &challenges[0] {
            AuthChallenge::Digest(d) => {
                assert_eq!(d.realm, "testrealm@host.com");
                assert_eq!(d.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
                assert_eq!(
                    d.opaque.as_deref(),
                    Some("5ccc069c403ebaf9f0171e9517f40e41")
                );
                assert_eq!(d.algorithm, DigestAlgorithm::Md5);
                assert_eq!(
                    d.qop.as_deref(),
                    Some(["auth".to_string(), "auth-int".to_string()].as_ref())
                );
            }
            _ => panic!("not a digest challenge"),
        }
    }

    #[test]
    fn build_md5_digest_response_rfc7616_example() {
        // RFC 7616 §3.9.1 example (MD5):
        //   user = "Mufasa", password = "Circle of Life"
        //   realm = "http-auth@example.org", nonce = "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v"
        //   uri = "/dir/index.html", method = "GET", qop = "auth"
        //   nc = 1, cnonce = "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ"
        //   expected response = "8ca523f5e9506fed4657c9700eebdbec"
        let challenge = DigestChallenge {
            realm: "http-auth@example.org".to_string(),
            nonce: "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v".to_string(),
            opaque: None,
            algorithm: DigestAlgorithm::Md5,
            qop: Some(vec!["auth".to_string()]),
            stale: false,
        };
        let pw = SecretString::new("Circle of Life".into());
        let ctx = DigestContext {
            username: "Mufasa",
            password: &pw,
            method: "GET",
            uri: "/dir/index.html",
            nc: 1,
            cnonce: "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            challenge: &challenge,
        };
        let header = build_digest_response(&ctx);
        assert!(
            header.contains(r#"response="8ca523f5e9506fed4657c9700eebdbec""#),
            "got: {}",
            header
        );
    }

    #[test]
    fn build_sha256_digest_response_rfc7616_example() {
        // RFC 7616 §3.9.1 example (SHA-256): expected response =
        // "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1"
        let challenge = DigestChallenge {
            realm: "http-auth@example.org".to_string(),
            nonce: "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v".to_string(),
            opaque: None,
            algorithm: DigestAlgorithm::Sha256,
            qop: Some(vec!["auth".to_string()]),
            stale: false,
        };
        let pw = SecretString::new("Circle of Life".into());
        let ctx = DigestContext {
            username: "Mufasa",
            password: &pw,
            method: "GET",
            uri: "/dir/index.html",
            nc: 1,
            cnonce: "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            challenge: &challenge,
        };
        let header = build_digest_response(&ctx);
        assert!(
            header.contains(
                r#"response="753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1""#
            ),
            "got: {}",
            header
        );
    }

    #[test]
    fn build_rfc2617_no_qop_response() {
        // Older cameras send Digest without qop. Verify we handle that.
        let challenge = DigestChallenge {
            realm: "OldCam".to_string(),
            nonce: "abc123".to_string(),
            opaque: None,
            algorithm: DigestAlgorithm::Md5,
            qop: None,
            stale: false,
        };
        let pw = SecretString::new("admin".into());
        let ctx = DigestContext {
            username: "admin",
            password: &pw,
            method: "OPTIONS",
            uri: "rtsp://cam/h264",
            nc: 1,
            cnonce: "deadbeef",
            challenge: &challenge,
        };
        let header = build_digest_response(&ctx);
        // RFC 2617 unqualified: response = H(HA1:nonce:HA2)
        // Header must NOT contain qop= or nc= or cnonce=
        assert!(!header.contains("qop="));
        assert!(!header.contains("nc="));
        assert!(!header.contains("cnonce="));
        assert!(header.starts_with(r#"Digest username="admin""#));
    }
}
