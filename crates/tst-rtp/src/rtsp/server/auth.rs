//! Server-side challenge generation + Authorization verification.
//!
//! Symmetric with the Phase 2 client-side primitives in
//! `crate::rtsp::auth`: the wire shapes of `WWW-Authenticate` and
//! `Authorization` are direct mirrors. Server emits challenges; client
//! emits responses; server verifies by recomputing the same Digest math.

use std::sync::atomic::{AtomicU32, Ordering};

use secrecy::{ExposeSecret, SecretString};

use crate::builder::{ServerAuthConfig, ServerAuthScheme};

/// Verification result. `Ok(())` means the request is authorized; any
/// error means the request handler should reply 401 (typically with a
/// fresh challenge).
#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthVerifyError {
    #[error("missing Authorization header")]
    Missing,
    #[error("unsupported auth scheme in Authorization header")]
    SchemeMismatch,
    #[error("base64 decode failed: {0}")]
    BadBasic(String),
    #[error("Basic credentials don't match")]
    WrongCredentials,
    #[error("malformed Digest Authorization header: {detail}")]
    BadDigestSyntax { detail: String },
    #[error("Digest response doesn't match expected")]
    WrongDigestResponse,
    #[error("Digest nonce mismatch (stale)")]
    StaleNonce,
}

/// Generate a fresh 16-byte nonce hex-encoded for use in a `WWW-Authenticate`
/// `nonce=` parameter. Rotated per session.
pub(crate) fn generate_nonce() -> String {
    let mut buf = [0u8; 16];
    if let Err(e) = getrandom::getrandom(&mut buf) {
        tracing::warn!(error = %e, "getrandom failed during nonce generation");
        // Fallback to a counter-based nonce; not great but won't allow
        // replay attacks within a session because we change it per-session.
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        buf[..4].copy_from_slice(&n.to_be_bytes());
    }
    let mut s = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Build the `WWW-Authenticate:` header value for the configured scheme.
///
/// `nonce` is only consumed for the Digest schemes; Basic ignores it.
pub(crate) fn build_challenge_header(cfg: &ServerAuthConfig, nonce: &str) -> String {
    match cfg.scheme {
        ServerAuthScheme::Basic => format!("Basic realm=\"{}\"", cfg.realm),
        ServerAuthScheme::DigestMd5 => format!(
            "Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5, qop=\"auth\"",
            cfg.realm, nonce
        ),
        ServerAuthScheme::DigestSha256 => format!(
            "Digest realm=\"{}\", nonce=\"{}\", algorithm=SHA-256, qop=\"auth\"",
            cfg.realm, nonce
        ),
    }
}

/// Verify the client's `Authorization:` header against the configured
/// credentials. Caller passes `Option<&str>` so the common "no header"
/// case maps directly to [`AuthVerifyError::Missing`].
///
/// For Digest auth, `expected_nonce` is the value the server most
/// recently emitted in `WWW-Authenticate: ... nonce=...` for this
/// session. Mismatches surface as `AuthVerifyError::StaleNonce`, which
/// the request handler should re-challenge with `stale=true`.
///
/// `nc_hwm` is the per-session nonce-count high-water mark used to
/// reject nc replays. It advances whenever a structurally valid Digest
/// attempt presents a fresh nc under the current nonce — deliberately
/// including attempts that then fail the password check, so a captured
/// Authorization header can never be replayed (see the note at the
/// update site in `verify_digest`). Reset it to 0 whenever the nonce
/// is rotated.
pub(crate) fn verify_authorization(
    auth_header: Option<&str>,
    method: &str,
    uri: &str,
    cfg: &ServerAuthConfig,
    expected_nonce: &str,
    nc_hwm: &mut u32,
) -> Result<(), AuthVerifyError> {
    let auth = auth_header.ok_or(AuthVerifyError::Missing)?;
    match cfg.scheme {
        ServerAuthScheme::Basic => verify_basic(auth, &cfg.username, &cfg.password),
        ServerAuthScheme::DigestMd5 | ServerAuthScheme::DigestSha256 => verify_digest(
            auth,
            method,
            uri,
            &cfg.username,
            &cfg.password,
            &cfg.realm,
            cfg.scheme,
            expected_nonce,
            nc_hwm,
        ),
    }
}

fn verify_basic(
    auth_header: &str,
    expected_user: &str,
    expected_pass: &SecretString,
) -> Result<(), AuthVerifyError> {
    let value = auth_header
        .strip_prefix("Basic ")
        .ok_or(AuthVerifyError::SchemeMismatch)?
        .trim();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| AuthVerifyError::BadBasic(e.to_string()))?;
    let s = std::str::from_utf8(&decoded).map_err(|e| AuthVerifyError::BadBasic(e.to_string()))?;
    let (user, pass) = s
        .split_once(':')
        .ok_or_else(|| AuthVerifyError::BadBasic("missing colon".to_string()))?;
    // Constant-time comparison for both user and pass. The two `Choice`
    // values are combined with `&` *before* converting to `bool` so that
    // subtle's optimizer barriers remain in effect across the entire
    // combined check — converting each to `bool` first would allow the
    // compiler to reason about branch-free composition in its own terms,
    // potentially breaking subtle's guarantees.
    //
    // Length-timing residual: subtle's slice `ct_eq` short-circuits when
    // the byte lengths differ, so a timing observer can infer whether the
    // submitted credential *length* matches the configured one. Lengths are
    // not treated as secret here (accepted residual; padding or hashing was
    // deliberately not applied).
    use subtle::ConstantTimeEq;
    let ok = user.as_bytes().ct_eq(expected_user.as_bytes())
        & pass
            .as_bytes()
            .ct_eq(expected_pass.expose_secret().as_bytes());
    if bool::from(ok) {
        Ok(())
    } else {
        Err(AuthVerifyError::WrongCredentials)
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_digest(
    auth_header: &str,
    method: &str,
    uri: &str,
    expected_user: &str,
    expected_pass: &SecretString,
    expected_realm: &str,
    scheme: ServerAuthScheme,
    expected_nonce: &str,
    nc_hwm: &mut u32,
) -> Result<(), AuthVerifyError> {
    let value = auth_header
        .strip_prefix("Digest ")
        .ok_or(AuthVerifyError::SchemeMismatch)?;
    let params = parse_kv_pairs(value);
    let username = params.get("username").map(String::as_str).unwrap_or("");
    let nonce_attr = params.get("nonce").map(String::as_str).unwrap_or("");
    let cnonce = params.get("cnonce").map(String::as_str).unwrap_or("");
    let nc_str = params.get("nc").map(String::as_str).unwrap_or("");
    let qop = params.get("qop").map(String::as_str).unwrap_or("");
    let uri_attr = params.get("uri").map(String::as_str).unwrap_or("");
    let response_attr = params.get("response").map(String::as_str).unwrap_or("");
    let realm_attr = params.get("realm").map(String::as_str).unwrap_or("");

    if username != expected_user {
        return Err(AuthVerifyError::WrongCredentials);
    }
    if nonce_attr != expected_nonce {
        return Err(AuthVerifyError::StaleNonce);
    }

    // DA-RTP-4(a): reject when the client's realm doesn't match ours.
    // A mismatched realm means HA1 was computed against a different domain;
    // the response will never verify. Surface as BadDigestSyntax (structural
    // violation of the challenge-response contract).
    if realm_attr != expected_realm {
        return Err(AuthVerifyError::BadDigestSyntax {
            detail: format!(
                "realm mismatch: client sent '{}', server realm is '{}'",
                realm_attr, expected_realm
            ),
        });
    }

    // DA-RTP-4(b): reject nc replays and regressions.
    // The server always offers qop=auth, so nc MUST be present when qop is
    // non-empty. If nc is absent or unparseable, treat as a syntax error.
    if !qop.is_empty() {
        let nc_val =
            u32::from_str_radix(nc_str, 16).map_err(|_| AuthVerifyError::BadDigestSyntax {
                detail: format!("nc '{nc_str}' is not a valid hex counter"),
            })?;
        if nc_val <= *nc_hwm {
            return Err(AuthVerifyError::BadDigestSyntax {
                detail: format!(
                    "nc {nc_val:#010x} replays or regresses hwm {:#010x}",
                    *nc_hwm
                ),
            });
        }
        // Deliberately advanced BEFORE the password check below: a request
        // with a valid nc but a wrong password still consumes that nc, so a
        // captured Authorization header can never be replayed regardless of
        // which check it failed. The cost — an unauthenticated peer can burn
        // nc values — is bounded by the auth_failures counter (connection
        // closes after 3 consecutive 401s) and by hwm reset on nonce rotation.
        *nc_hwm = nc_val;
    }

    // DA-RTP-4(c): reject the RFC 2617 no-qop downgrade when the server
    // challenge offered qop=auth. Our build_challenge_header always adds
    // qop="auth" for DigestMd5/DigestSha256; a client that omits qop is
    // deliberately downgrading (or is a broken implementation).
    if qop.is_empty() {
        return Err(AuthVerifyError::BadDigestSyntax {
            detail: "qop downgrade rejected: server offered qop=auth".to_string(),
        });
    }

    if uri_attr != uri {
        // RFC 7616 §3.4.6 — URI in Authorization MUST match request URI.
        return Err(AuthVerifyError::BadDigestSyntax {
            detail: format!("uri attr '{uri_attr}' != request uri '{uri}'"),
        });
    }
    let expected_response = compute_digest_response(
        scheme,
        expected_user,
        expected_realm,
        expected_pass,
        method,
        uri,
        nonce_attr,
        nc_str,
        cnonce,
        qop,
    );
    // Constant-time comparison for the Digest response to prevent timing
    // attacks that could reveal partial information about the expected hash.
    use subtle::ConstantTimeEq;
    let response_ok: bool = response_attr
        .as_bytes()
        .ct_eq(expected_response.as_bytes())
        .into();
    if response_ok {
        Ok(())
    } else {
        Err(AuthVerifyError::WrongDigestResponse)
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_digest_response(
    scheme: ServerAuthScheme,
    user: &str,
    realm: &str,
    pass: &SecretString,
    method: &str,
    uri: &str,
    nonce: &str,
    nc: &str,
    cnonce: &str,
    qop: &str,
) -> String {
    use crate::rtsp::digest::{Algo, ha1, response};
    let algo = match scheme {
        ServerAuthScheme::DigestMd5 => Algo::Md5,
        ServerAuthScheme::DigestSha256 => Algo::Sha256,
        ServerAuthScheme::Basic => unreachable!("compute_digest_response not called for Basic"),
    };
    let computed_ha1 = ha1(algo, user, realm, pass);
    response(algo, &computed_ha1, method, uri, nonce, nc, cnonce, qop)
}

/// Reverse of the client-side `escape_quoted_string` (`crate::rtsp::auth`):
/// a `\` inside a quoted-string escapes the character that follows it
/// (RFC 7230 §3.2.6 `quoted-pair`, shared by RFC 7616 §3.4) — drop the
/// backslash and keep that character literally. `parse_kv_pairs`'s scanner
/// already walks past every `\X` pair to find the closing `"`; this makes
/// the STORED value match what the client actually meant, not the raw
/// escaped wire bytes — e.g. a percent-decoded username `a"b` (CORR-21)
/// arrives as `\"` inside `username="a\"b"` and must compare equal to the
/// configured `a"b`, not the literal three bytes `a\"b`.
fn unescape_quoted_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
                continue;
            }
            // Trailing lone backslash (malformed input) — keep it as-is
            // rather than silently dropping a byte.
        }
        out.push(c);
    }
    out
}

/// Tokenize a Digest parameter list `key="value", key2=token, ...` into a
/// HashMap. Tolerant of optional whitespace around `=` and `,`. Quoted
/// values may contain commas; backslash-escapes inside quoted strings are
/// handled per RFC 7616 §3.4 — the stored value is UN-escaped (`\"` -> `"`,
/// `\\` -> `\`), not the raw wire bytes.
fn parse_kv_pairs(input: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b',' {
            i += 1;
            continue;
        }
        let key = input[key_start..i].trim().to_ascii_lowercase();
        i += 1; // skip '='
        if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                i += 1;
            }
            out.insert(key, unescape_quoted_string(&input[val_start..i]));
            if i < bytes.len() {
                i += 1;
            }
        } else {
            let val_start = i;
            while i < bytes.len() && bytes[i] != b',' && bytes[i] != b' ' {
                i += 1;
            }
            out.insert(key, input[val_start..i].to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    fn basic_cfg() -> ServerAuthConfig {
        ServerAuthConfig {
            scheme: ServerAuthScheme::Basic,
            realm: "tst".into(),
            username: "admin".into(),
            password: SecretString::new("secret".into()),
        }
    }

    fn digest_md5_cfg() -> ServerAuthConfig {
        ServerAuthConfig {
            scheme: ServerAuthScheme::DigestMd5,
            realm: "tst-rtp".into(),
            username: "admin".into(),
            password: SecretString::new("secret".into()),
        }
    }

    #[test]
    fn generate_nonce_is_32_hex_chars() {
        let n = generate_nonce();
        assert_eq!(n.len(), 32);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_nonce_changes_each_call() {
        let a = generate_nonce();
        let b = generate_nonce();
        assert_ne!(a, b);
    }

    #[test]
    fn challenge_header_basic() {
        let cfg = basic_cfg();
        let ch = build_challenge_header(&cfg, "ignored");
        assert_eq!(ch, "Basic realm=\"tst\"");
    }

    #[test]
    fn challenge_header_digest_md5() {
        let cfg = digest_md5_cfg();
        let ch = build_challenge_header(&cfg, "abc123");
        assert!(ch.contains("Digest"));
        assert!(ch.contains("realm=\"tst-rtp\""));
        assert!(ch.contains("nonce=\"abc123\""));
        assert!(ch.contains("algorithm=MD5"));
        assert!(ch.contains("qop=\"auth\""));
    }

    #[test]
    fn challenge_header_digest_sha256() {
        let cfg = ServerAuthConfig {
            scheme: ServerAuthScheme::DigestSha256,
            ..digest_md5_cfg()
        };
        let ch = build_challenge_header(&cfg, "xyz789");
        assert!(ch.contains("algorithm=SHA-256"));
    }

    #[test]
    fn verify_missing_authorization() {
        let cfg = basic_cfg();
        let e = verify_authorization(None, "DESCRIBE", "rtsp://x", &cfg, "", &mut 0).unwrap_err();
        assert!(matches!(e, AuthVerifyError::Missing));
    }

    #[test]
    fn verify_basic_round_trip() {
        let cfg = basic_cfg();
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"admin:secret")
        );
        verify_authorization(
            Some(&auth),
            "DESCRIBE",
            "rtsp://server/live",
            &cfg,
            "",
            &mut 0,
        )
        .unwrap();
    }

    #[test]
    fn verify_basic_wrong_password() {
        let cfg = basic_cfg();
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"admin:wrong")
        );
        let e = verify_authorization(
            Some(&auth),
            "DESCRIBE",
            "rtsp://server/live",
            &cfg,
            "",
            &mut 0,
        )
        .unwrap_err();
        assert!(matches!(e, AuthVerifyError::WrongCredentials));
    }

    #[test]
    fn verify_basic_wrong_username() {
        let cfg = basic_cfg();
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"wrong:secret")
        );
        let e = verify_authorization(
            Some(&auth),
            "DESCRIBE",
            "rtsp://server/live",
            &cfg,
            "",
            &mut 0,
        )
        .unwrap_err();
        assert!(matches!(e, AuthVerifyError::WrongCredentials));
    }

    // ── Behavior-pinning tests for constant-time comparison ───────────────
    // These must pass both before and after the ct-comparison refactor.

    /// Both username and password wrong → still `WrongCredentials` (not panic,
    /// not a different variant).
    #[test]
    fn verify_basic_wrong_both_returns_wrong_credentials() {
        let cfg = basic_cfg();
        use base64::Engine;
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(b"nobody:badpass")
        );
        let e = verify_authorization(
            Some(&auth),
            "DESCRIBE",
            "rtsp://server/live",
            &cfg,
            "",
            &mut 0,
        )
        .unwrap_err();
        assert!(
            matches!(e, AuthVerifyError::WrongCredentials),
            "wrong user+pass must give WrongCredentials, got {e:?}"
        );
    }

    /// Credentials whose byte lengths differ from the expected values must
    /// not panic and must produce `WrongCredentials`.
    #[test]
    fn verify_basic_differing_length_no_panic() {
        let cfg = basic_cfg(); // password = "secret" (6 bytes)
        use base64::Engine;
        // Pass a much longer password so the length comparison is exercised.
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(b"admin:this-is-a-much-longer-password-than-expected")
        );
        let e = verify_authorization(
            Some(&auth),
            "DESCRIBE",
            "rtsp://server/live",
            &cfg,
            "",
            &mut 0,
        )
        .unwrap_err();
        assert!(
            matches!(e, AuthVerifyError::WrongCredentials),
            "longer-than-expected password must give WrongCredentials without panic, got {e:?}"
        );
    }

    #[test]
    fn verify_basic_bad_base64() {
        let cfg = basic_cfg();
        let auth = "Basic not-base64!!";
        let e =
            verify_authorization(Some(auth), "DESCRIBE", "rtsp://x", &cfg, "", &mut 0).unwrap_err();
        assert!(matches!(e, AuthVerifyError::BadBasic(_)));
    }

    #[test]
    fn verify_scheme_mismatch() {
        let cfg = basic_cfg();
        let auth = "Bearer some-token-here";
        let e =
            verify_authorization(Some(auth), "DESCRIBE", "rtsp://x", &cfg, "", &mut 0).unwrap_err();
        assert!(matches!(e, AuthVerifyError::SchemeMismatch));
    }

    #[test]
    fn verify_digest_md5_round_trip() {
        let cfg = digest_md5_cfg();
        let nonce = "abc123";
        let nc = "00000001";
        let cnonce = "deadbeef";
        let qop = "auth";
        let method = "DESCRIBE";
        let uri = "rtsp://server/live";
        let response = compute_digest_response(
            ServerAuthScheme::DigestMd5,
            "admin",
            "tst-rtp",
            &cfg.password,
            method,
            uri,
            nonce,
            nc,
            cnonce,
            qop,
        );
        let auth = format!(
            "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"{nonce}\", \
             uri=\"{uri}\", response=\"{response}\", algorithm=MD5, \
             nc={nc}, cnonce=\"{cnonce}\", qop={qop}"
        );
        verify_authorization(Some(&auth), method, uri, &cfg, nonce, &mut 0).unwrap();
    }

    #[test]
    fn verify_digest_sha256_round_trip() {
        let cfg = ServerAuthConfig {
            scheme: ServerAuthScheme::DigestSha256,
            ..digest_md5_cfg()
        };
        let nonce = "abc123";
        let response = compute_digest_response(
            ServerAuthScheme::DigestSha256,
            "admin",
            "tst-rtp",
            &cfg.password,
            "DESCRIBE",
            "rtsp://server/live",
            nonce,
            "00000001",
            "cnonce123",
            "auth",
        );
        let auth = format!(
            "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"{nonce}\", \
             uri=\"rtsp://server/live\", response=\"{response}\", algorithm=SHA-256, \
             nc=00000001, cnonce=\"cnonce123\", qop=auth"
        );
        verify_authorization(
            Some(&auth),
            "DESCRIBE",
            "rtsp://server/live",
            &cfg,
            nonce,
            &mut 0,
        )
        .unwrap();
    }

    #[test]
    fn verify_digest_stale_nonce() {
        let cfg = digest_md5_cfg();
        let auth = "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"stale\", \
                    uri=\"rtsp://x\", response=\"any\", algorithm=MD5";
        let e = verify_authorization(Some(auth), "DESCRIBE", "rtsp://x", &cfg, "fresh", &mut 0)
            .unwrap_err();
        assert!(matches!(e, AuthVerifyError::StaleNonce));
    }

    #[test]
    fn verify_digest_wrong_username() {
        let cfg = digest_md5_cfg();
        let auth = "Digest username=\"impostor\", realm=\"tst-rtp\", nonce=\"abc\", \
                    uri=\"rtsp://x\", response=\"any\", algorithm=MD5";
        let e = verify_authorization(Some(auth), "DESCRIBE", "rtsp://x", &cfg, "abc", &mut 0)
            .unwrap_err();
        assert!(matches!(e, AuthVerifyError::WrongCredentials));
    }

    #[test]
    fn verify_digest_uri_mismatch() {
        let cfg = digest_md5_cfg();
        let nonce = "abc";
        let response = compute_digest_response(
            ServerAuthScheme::DigestMd5,
            "admin",
            "tst-rtp",
            &cfg.password,
            "DESCRIBE",
            "rtsp://x",
            nonce,
            "00000001",
            "cnonce",
            "auth",
        );
        // Client sent uri="rtsp://y" but request line says "rtsp://x".
        let auth = format!(
            "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"{nonce}\", \
             uri=\"rtsp://y\", response=\"{response}\", algorithm=MD5, \
             nc=00000001, cnonce=\"cnonce\", qop=auth"
        );
        let e = verify_authorization(Some(&auth), "DESCRIBE", "rtsp://x", &cfg, nonce, &mut 0)
            .unwrap_err();
        assert!(matches!(e, AuthVerifyError::BadDigestSyntax { .. }));
    }

    // ── DA-RTP-4 hardening tests ────────────────────────────────────────

    /// DA-RTP-4(a): client supplies a realm that differs from the server's
    /// configured realm — the response would be computed against the wrong
    /// domain. Expect BadDigestSyntax.
    #[test]
    fn verify_digest_realm_mismatch_rejected() {
        let cfg = digest_md5_cfg(); // realm = "tst-rtp"
        let nonce = "nonce1";
        // Auth header with realm="wrong-realm" (not "tst-rtp").
        let auth = format!(
            "Digest username=\"admin\", realm=\"wrong-realm\", nonce=\"{nonce}\", \
             uri=\"rtsp://x\", response=\"any\", algorithm=MD5, \
             nc=00000001, cnonce=\"cc\", qop=auth"
        );
        let e = verify_authorization(Some(&auth), "DESCRIBE", "rtsp://x", &cfg, nonce, &mut 0)
            .unwrap_err();
        assert!(
            matches!(e, AuthVerifyError::BadDigestSyntax { .. }),
            "expected BadDigestSyntax for realm mismatch, got {e:?}"
        );
    }

    /// DA-RTP-4(b): nc replay — sending nc=00000001 twice with the same
    /// nonce must be rejected.
    #[test]
    fn verify_digest_nc_replay_rejected() {
        let cfg = digest_md5_cfg();
        let nonce = "nonce1";
        let nc = "00000001";
        let cnonce = "cc";
        let method = "DESCRIBE";
        let uri = "rtsp://server/live";
        let response = compute_digest_response(
            ServerAuthScheme::DigestMd5,
            "admin",
            "tst-rtp",
            &cfg.password,
            method,
            uri,
            nonce,
            nc,
            cnonce,
            "auth",
        );
        let auth = format!(
            "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"{nonce}\", \
             uri=\"{uri}\", response=\"{response}\", algorithm=MD5, \
             nc={nc}, cnonce=\"{cnonce}\", qop=auth"
        );

        // First request: nc=1, hwm starts at 0 → accepted, hwm becomes 1.
        let mut hwm = 0u32;
        verify_authorization(Some(&auth), method, uri, &cfg, nonce, &mut hwm).unwrap();
        assert_eq!(hwm, 1, "hwm should be updated to 1 after first request");

        // Second request: nc=1 again (replay) → rejected.
        let e = verify_authorization(Some(&auth), method, uri, &cfg, nonce, &mut hwm).unwrap_err();
        assert!(
            matches!(e, AuthVerifyError::BadDigestSyntax { .. }),
            "expected BadDigestSyntax for nc replay, got {e:?}"
        );
        assert_eq!(hwm, 1, "hwm must not advance on rejection");
    }

    /// DA-RTP-4(b): nc must advance monotonically (regression rejected).
    #[test]
    fn verify_digest_nc_regression_rejected() {
        let cfg = digest_md5_cfg();
        let nonce = "nonce1";
        let method = "DESCRIBE";
        let uri = "rtsp://server/live";

        // Advance the hwm to 5 manually (simulating 5 prior requests).
        let mut hwm = 5u32;

        // Send nc=00000003 (regression below hwm=5) → rejected.
        let nc = "00000003";
        let response = compute_digest_response(
            ServerAuthScheme::DigestMd5,
            "admin",
            "tst-rtp",
            &cfg.password,
            method,
            uri,
            nonce,
            nc,
            "cc",
            "auth",
        );
        let auth = format!(
            "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"{nonce}\", \
             uri=\"{uri}\", response=\"{response}\", algorithm=MD5, \
             nc={nc}, cnonce=\"cc\", qop=auth"
        );
        let e = verify_authorization(Some(&auth), method, uri, &cfg, nonce, &mut hwm).unwrap_err();
        assert!(
            matches!(e, AuthVerifyError::BadDigestSyntax { .. }),
            "expected BadDigestSyntax for nc regression, got {e:?}"
        );
        assert_eq!(hwm, 5, "hwm must not change on rejected request");
    }

    /// DA-RTP-4(c): no-qop downgrade is rejected even with correct
    /// credentials — the server always offers qop=auth.
    #[test]
    fn verify_digest_qop_downgrade_rejected() {
        let cfg = digest_md5_cfg();
        let nonce = "nonce1";
        // RFC 2617 no-qop form: response = H(HA1:nonce:HA2), no nc/cnonce/qop.
        let auth = format!(
            "Digest username=\"admin\", realm=\"tst-rtp\", nonce=\"{nonce}\", \
             uri=\"rtsp://x\", response=\"any\", algorithm=MD5"
        );
        let e = verify_authorization(Some(&auth), "DESCRIBE", "rtsp://x", &cfg, nonce, &mut 0)
            .unwrap_err();
        assert!(
            matches!(e, AuthVerifyError::BadDigestSyntax { .. }),
            "expected BadDigestSyntax for qop downgrade, got {e:?}"
        );
    }

    #[test]
    fn parse_kv_pairs_quoted_with_commas() {
        let m = parse_kv_pairs(r#"a="hello, world", b=token"#);
        assert_eq!(m.get("a").map(String::as_str), Some("hello, world"));
        assert_eq!(m.get("b").map(String::as_str), Some("token"));
    }

    #[test]
    fn parse_kv_pairs_escaped_quotes() {
        // The stored value is UN-escaped per RFC 7616 §3.4's quoted-pair
        // rule: `\"` -> `"` (the scanner still uses the raw `\"` to find
        // the correct closing `"`, but what's stored is what the client
        // MEANT, not the wire bytes). A percent-decoded username
        // (CORR-21) can carry a literal `"` this way.
        let m = parse_kv_pairs(r#"a="he said \"hi\"", b=ok"#);
        assert_eq!(m.get("a").map(String::as_str), Some(r#"he said "hi""#));
        assert_eq!(m.get("b").map(String::as_str), Some("ok"));
    }

    /// CORR-21 follow-on: a username containing `"` must round-trip
    /// through `parse_kv_pairs` back to its real value, not the escaped
    /// wire form — `escape_quoted_string` (`crate::rtsp::auth`, client
    /// side) and this function are inverses.
    #[test]
    fn parse_kv_pairs_unescapes_quoted_username() {
        let m = parse_kv_pairs(r#"username="a\"b", uri="rtsp://cam/x""#);
        assert_eq!(m.get("username").map(String::as_str), Some(r#"a"b"#));
        assert_eq!(m.get("uri").map(String::as_str), Some("rtsp://cam/x"));
    }

    // ── WP14 M3: combined client-build → server-verify round-trip ─────────

    /// Exercise the PUBLIC `build_digest_response` client builder against the
    /// server verifier. Previously each side was tested only against shared RFC
    /// vectors; this helper drives the full path.
    fn client_server_roundtrip(
        algorithm: crate::rtsp::auth::DigestAlgorithm,
        scheme: ServerAuthScheme,
        client_password: &str,
        server_password: &str,
    ) -> (Result<(), AuthVerifyError>, u32) {
        use crate::rtsp::auth::{DigestChallenge, DigestContext, build_digest_response};
        let nonce = "0123456789abcdef0123456789abcdef";
        let challenge = DigestChallenge {
            realm: "tstrans-test".to_string(),
            nonce: nonce.to_string(),
            opaque: None,
            algorithm,
            qop: Some(vec!["auth".to_string()]),
            stale: false,
        };
        let password = SecretString::new(client_password.to_string().into());
        let ctx = DigestContext {
            username: "operator",
            password: &password,
            method: "DESCRIBE",
            uri: "rtsp://127.0.0.1/stream",
            nc: 1,
            cnonce: "abcdef01",
            challenge: &challenge,
        };
        let header = build_digest_response(&ctx);
        let cfg = ServerAuthConfig {
            scheme,
            realm: "tstrans-test".to_string(),
            username: "operator".to_string(),
            password: SecretString::new(server_password.to_string().into()),
        };
        let mut nc_hwm = 0u32;
        let res = verify_authorization(
            Some(&header),
            "DESCRIBE",
            "rtsp://127.0.0.1/stream",
            &cfg,
            nonce,
            &mut nc_hwm,
        );
        (res, nc_hwm)
    }

    #[test]
    fn client_built_md5_digest_verifies_server_side() {
        use crate::rtsp::auth::DigestAlgorithm;
        let (res, nc_hwm) = client_server_roundtrip(
            DigestAlgorithm::Md5,
            ServerAuthScheme::DigestMd5,
            "s3cret",
            "s3cret",
        );
        res.expect("server must accept the client-built MD5 digest");
        assert_eq!(
            nc_hwm, 1,
            "nc high-water mark must advance to the client's nc"
        );
    }

    #[test]
    fn client_built_sha256_digest_verifies_server_side() {
        use crate::rtsp::auth::DigestAlgorithm;
        let (res, nc_hwm) = client_server_roundtrip(
            DigestAlgorithm::Sha256,
            ServerAuthScheme::DigestSha256,
            "s3cret",
            "s3cret",
        );
        res.expect("server must accept the client-built SHA-256 digest");
        assert_eq!(
            nc_hwm, 1,
            "nc high-water mark must advance to the client's nc"
        );
    }

    #[test]
    fn client_built_digest_wrong_password_rejected() {
        use crate::rtsp::auth::DigestAlgorithm;
        let (res, _) = client_server_roundtrip(
            DigestAlgorithm::Md5,
            ServerAuthScheme::DigestMd5,
            "wrong",
            "s3cret",
        );
        assert!(
            matches!(res, Err(AuthVerifyError::WrongDigestResponse)),
            "mismatched password must be rejected, got {res:?}"
        );
    }
}
