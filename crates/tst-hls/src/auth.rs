//! HTTP Basic auth (RFC 7617) check for the HLS server.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Check whether the `Authorization: Basic ...` header matches the expected
/// (user, password).  Returns `true` on match, `false` on mismatch / absent /
/// malformed.
pub(crate) fn check_basic_auth(
    expected_user: &str,
    expected_pass: &str,
    header_value: Option<&str>,
) -> bool {
    let header = match header_value {
        Some(h) => h,
        None => return false,
    };
    let Some(b64) = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))
    else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(s) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((user, pass)) = s.split_once(':') else {
        return false;
    };
    // Constant-time comparison for both user and pass — same shape as
    // tst-rtp's RTSP `verify_basic`. The two `Choice`s are combined with `&`
    // BEFORE converting to `bool` so subtle's optimizer barriers cover the
    // whole check. Length-timing residual: slice `ct_eq` short-circuits on a
    // length mismatch, so an observer can learn whether the submitted
    // credential LENGTH matches; lengths are not treated as secret here
    // (accepted residual, same as tst-rtp).
    use subtle::ConstantTimeEq;
    let ok = user.as_bytes().ct_eq(expected_user.as_bytes())
        & pass.as_bytes().ct_eq(expected_pass.as_bytes());
    bool::from(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        STANDARD.encode(s.as_bytes())
    }

    #[test]
    fn correct_credentials_match() {
        let h = format!("Basic {}", b64("alice:s3cret"));
        assert!(check_basic_auth("alice", "s3cret", Some(&h)));
    }

    #[test]
    fn wrong_password_rejected() {
        let h = format!("Basic {}", b64("alice:wrong"));
        assert!(!check_basic_auth("alice", "s3cret", Some(&h)));
    }

    #[test]
    fn absent_header_rejected() {
        assert!(!check_basic_auth("alice", "s3cret", None));
    }

    #[test]
    fn malformed_b64_rejected() {
        assert!(!check_basic_auth("alice", "s3cret", Some("Basic !@#$")));
    }

    #[test]
    fn wrong_scheme_rejected() {
        let h = format!("Bearer {}", b64("alice:s3cret"));
        assert!(!check_basic_auth("alice", "s3cret", Some(&h)));
    }

    /// subtle's slice `ct_eq` returns `Choice(0)` when lengths differ (no
    /// panic, no early `true`); pin that a length mismatch on either field
    /// still rejects.
    #[test]
    fn mismatched_length_password_rejected() {
        let h = format!("Basic {}", b64("alice:s3cretXX"));
        assert!(!check_basic_auth("alice", "s3cret", Some(&h)));
        let h = format!("Basic {}", b64("alice:s3c"));
        assert!(!check_basic_auth("alice", "s3cret", Some(&h)));
    }

    #[test]
    fn mismatched_length_user_rejected() {
        let h = format!("Basic {}", b64("alicia:s3cret"));
        assert!(!check_basic_auth("alice", "s3cret", Some(&h)));
    }

    #[test]
    fn empty_password_only_matches_empty() {
        let h = format!("Basic {}", b64("alice:"));
        assert!(check_basic_auth("alice", "", Some(&h)));
        assert!(!check_basic_auth("alice", "s3cret", Some(&h)));
    }
}
