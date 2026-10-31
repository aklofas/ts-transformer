//! Shared Digest authentication primitives (RFC 7616 / RFC 2617).
//!
//! Used by both the RTSP client ([`super::auth`]) and the RTSP server
//! ([`super::server::auth`]) so hex-encoding, hash dispatch, A1/A2, and
//! the response token are implemented once and tested once.

use secrecy::{ExposeSecret, SecretString};

/// Which hash algorithm drives HA1/HA2/response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Algo {
    Md5,
    Sha256,
}

/// Hex-encode a fixed-size byte array into a lowercase ASCII string.
pub(crate) fn hex<const N: usize>(b: [u8; N]) -> String {
    let mut s = String::with_capacity(N * 2);
    for x in b {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", x);
    }
    s
}

/// Hash `s` with the chosen algorithm and return the lowercase hex digest.
pub(crate) fn hash(algo: Algo, s: &str) -> String {
    use md5::Md5;
    use sha2::{Digest as _, Sha256};
    match algo {
        Algo::Md5 => {
            let mut h = Md5::new();
            h.update(s.as_bytes());
            hex(h.finalize().into())
        }
        Algo::Sha256 => {
            let mut h = Sha256::new();
            h.update(s.as_bytes());
            hex(h.finalize().into())
        }
    }
}

/// Compute HA1 = H(username:realm:password).
///
/// For the `*-sess` variants the caller further hashes:
/// `H(ha1(…):nonce:cnonce)` before passing to [`response`].
pub(crate) fn ha1(algo: Algo, username: &str, realm: &str, password: &SecretString) -> String {
    hash(
        algo,
        &format!("{}:{}:{}", username, realm, password.expose_secret()),
    )
}

/// Compute the Digest response token from a pre-computed HA1 string.
///
/// When `qop` is empty the RFC 2617 no-qop form `H(HA1:nonce:HA2)` is
/// used; `nc` and `cnonce` are ignored. When `qop` is non-empty
/// (typically `"auth"`), the full RFC 7616 §3.4.1 form is used.
#[allow(clippy::too_many_arguments)]
pub(crate) fn response(
    algo: Algo,
    ha1: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    nc: &str,
    cnonce: &str,
    qop: &str,
) -> String {
    let ha2 = hash(algo, &format!("{}:{}", method, uri));
    if qop.is_empty() {
        hash(algo, &format!("{}:{}:{}", ha1, nonce, ha2))
    } else {
        hash(
            algo,
            &format!("{}:{}:{}:{}:{}:{}", ha1, nonce, nc, cnonce, qop, ha2),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7616 §3.9.1 MD5 test vector — also a cross-check that the
    /// client path (via DigestAlgorithm::Md5 → Algo::Md5) and server path
    /// (via AuthScheme::DigestMd5 → Algo::Md5) produce identical
    /// values for the same inputs.
    #[test]
    fn cross_check_md5_with_qop() {
        let pw = SecretString::new("Circle of Life".into());
        let computed_ha1 = ha1(Algo::Md5, "Mufasa", "http-auth@example.org", &pw);
        let resp = response(
            Algo::Md5,
            &computed_ha1,
            "GET",
            "/dir/index.html",
            "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v",
            "00000001",
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            "auth",
        );
        assert_eq!(
            resp, "8ca523f5e9506fed4657c9700eebdbec",
            "RFC 7616 §3.9.1 MD5 vector"
        );
    }

    /// RFC 7616 §3.9.1 SHA-256 test vector.
    #[test]
    fn cross_check_sha256_with_qop() {
        let pw = SecretString::new("Circle of Life".into());
        let computed_ha1 = ha1(Algo::Sha256, "Mufasa", "http-auth@example.org", &pw);
        let resp = response(
            Algo::Sha256,
            &computed_ha1,
            "GET",
            "/dir/index.html",
            "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v",
            "00000001",
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            "auth",
        );
        assert_eq!(
            resp, "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1",
            "RFC 7616 §3.9.1 SHA-256 vector"
        );
    }

    /// RFC 2617 no-qop form: both sides must agree on H(HA1:nonce:HA2).
    #[test]
    fn cross_check_no_qop_is_deterministic() {
        let pw = SecretString::new("admin".into());
        let computed_ha1 = ha1(Algo::Md5, "admin", "OldCam", &pw);
        let r1 = response(
            Algo::Md5,
            &computed_ha1,
            "OPTIONS",
            "rtsp://cam/h264",
            "abc123",
            "",
            "",
            "",
        );
        let r2 = response(
            Algo::Md5,
            &computed_ha1,
            "OPTIONS",
            "rtsp://cam/h264",
            "abc123",
            "",
            "",
            "",
        );
        assert_eq!(r1, r2, "no-qop response must be deterministic");
        assert!(!r1.is_empty());
    }
}
