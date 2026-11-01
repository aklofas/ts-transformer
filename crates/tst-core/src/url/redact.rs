//! Redaction of secrets from URL text before it reaches a log, a `Debug`
//! impl, a `repr` or an error message.

use alloc::string::String;

/// Query keys whose values are secrets. Matched exactly, in lower case.
const SECRET_KEYS: [&str; 5] = ["passphrase", "secret", "token", "auth_pass", "password"];

const REDACTED: &str = "<redacted>";

/// Return `url` with its secrets replaced by `<redacted>`.
///
/// Two things are scrubbed; everything else (scheme, user name, host, port,
/// path, other query values) is kept verbatim:
///
/// - the password of `user:password@host` userinfo;
/// - the value of a `key=value` pair whose key is exactly one of
///   `passphrase`, `secret`, `token`, `auth_pass`, `password`. The match is
///   case-sensitive: `Token=x` is not scrubbed.
///
/// The input need not be a valid URL. The scrub is a text pass over each
/// whitespace- or quote-delimited token, so a malformed URL, or a longer
/// message that embeds one, never leaks either kind of secret.
///
/// ```
/// use tst_core::url::redact_url;
/// assert_eq!(
///     redact_url("rtsp://cam:hunter2@10.0.0.5:554/s?passphrase=abc&x=1"),
///     "rtsp://cam:<redacted>@10.0.0.5:554/s?passphrase=<redacted>&x=1"
/// );
/// ```
#[must_use]
pub fn redact_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut start = 0;
    for (i, c) in url.char_indices() {
        if c.is_whitespace() || c == '"' || c == '\'' {
            redact_token(&url[start..i], &mut out);
            out.push(c);
            start = i + c.len_utf8();
        }
    }
    redact_token(&url[start..], &mut out);
    out
}

fn redact_token(token: &str, out: &mut String) {
    let (head, rest) = match token.find("://") {
        Some(i) => token.split_at(i + 3),
        None => ("", token),
    };
    let mut scrubbed = String::with_capacity(token.len());
    scrubbed.push_str(head);
    redact_userinfo(rest, &mut scrubbed);
    redact_query_values(&scrubbed, out);
}

/// Redact the password in the userinfo that precedes the last `@` before
/// any `?`. The password may itself contain `/` or `@`, so the split is
/// taken from the right, as a client would not and a leak would.
fn redact_userinfo(rest: &str, out: &mut String) {
    let limit = rest.find('?').unwrap_or(rest.len());
    if let Some(at) = rest[..limit].rfind('@')
        && let Some(colon) = rest[..at].find(':')
    {
        out.push_str(&rest[..=colon]);
        out.push_str(REDACTED);
        out.push_str(&rest[at..]);
    } else {
        out.push_str(rest);
    }
}

fn redact_query_values(token: &str, out: &mut String) {
    let bytes = token.as_bytes();
    let mut i = 0;
    let mut copied = 0;
    while i < bytes.len() {
        let at_boundary = i == 0 || !is_word_byte(bytes[i - 1]);
        let key = at_boundary
            .then(|| {
                SECRET_KEYS.iter().find(|k| {
                    token[i..]
                        .strip_prefix(**k)
                        .is_some_and(|r| r.starts_with('='))
                })
            })
            .flatten();
        if let Some(key) = key {
            let value_start = i + key.len() + 1;
            let value_end = token[value_start..]
                .find('&')
                .map_or(token.len(), |n| value_start + n);
            out.push_str(&token[copied..value_start]);
            out.push_str(REDACTED);
            copied = value_end;
            i = value_end;
        } else {
            i += 1;
        }
    }
    out.push_str(&token[copied..]);
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

#[cfg(test)]
mod tests {
    use super::redact_url;

    #[test]
    fn userinfo_password_is_redacted_user_kept() {
        assert_eq!(
            redact_url("rtsp://cam:hunter2@10.0.0.5:554/s"),
            "rtsp://cam:<redacted>@10.0.0.5:554/s"
        );
    }

    #[test]
    fn userinfo_without_password_is_untouched() {
        assert_eq!(redact_url("rtsp://cam@h:554/s"), "rtsp://cam@h:554/s");
        assert_eq!(
            redact_url("srt://h:9000?latency=120"),
            "srt://h:9000?latency=120"
        );
    }

    #[test]
    fn password_containing_slash_and_at_is_redacted() {
        let r = redact_url("rtsp://u:pa/ss@word@h/x");
        assert_eq!(r, "rtsp://u:<redacted>@h/x");
    }

    #[test]
    fn each_secret_key_is_redacted() {
        for k in ["passphrase", "secret", "token", "auth_pass", "password"] {
            let r = redact_url(&format!("srt://h:1?a=b&{k}=s3cr3t&c=d"));
            assert_eq!(r, format!("srt://h:1?a=b&{k}=<redacted>&c=d"));
        }
    }

    #[test]
    fn first_query_key_and_other_values_verbatim() {
        assert_eq!(
            redact_url("rist://h:1?secret=abc&latency=50"),
            "rist://h:1?secret=<redacted>&latency=50"
        );
    }

    #[test]
    fn key_match_is_exact_lowercase() {
        assert_eq!(redact_url("srt://h:1?Token=abc"), "srt://h:1?Token=abc");
        assert_eq!(
            redact_url("srt://h:1?client_secret=abc"),
            "srt://h:1?client_secret=abc"
        );
    }

    #[test]
    fn non_url_text_is_scrubbed() {
        assert_eq!(redact_url("passphrase=abc"), "passphrase=<redacted>");
        assert_eq!(redact_url("u:pw@h"), "u:<redacted>@h");
        assert_eq!(
            redact_url("bad url 'srt://h?passphrase=abc' here"),
            "bad url 'srt://h?passphrase=<redacted>' here"
        );
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(redact_url("hello world"), "hello world");
        assert_eq!(redact_url(""), "");
        assert_eq!(redact_url("user@host"), "user@host");
    }
}
