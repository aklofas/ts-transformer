//! Redaction of secrets from URL text before it reaches a log, a `Debug`
//! impl, a `repr` or an error message.

use alloc::string::String;
use alloc::vec::Vec;

/// Query keys whose values are secrets. Matched exactly, in lower case.
const SECRET_KEYS: [&str; 7] = [
    "passphrase",
    "secret",
    "token",
    "auth_pass",
    "password",
    "streamid",
    "srt_streamid",
];

const REDACTED: &str = "<redacted>";

/// Return `url` with its secrets replaced by `<redacted>`.
///
/// Two things are scrubbed; everything else (scheme, user name, host, port,
/// path, other query values) is kept verbatim:
///
/// - the password of `user:password@host` userinfo;
/// - the value of a `key=value` pair whose key is exactly one of
///   `passphrase`, `secret`, `token`, `auth_pass`, `password`, `streamid`,
///   `srt_streamid`. The match is case-sensitive: `Token=x` is not scrubbed. Key
///   names are percent-decoded first, so `pass%70hrase=x` is scrubbed too.
///
/// The scrub errs toward redacting: any later `@` ends the userinfo, so
/// `srt://h:9000/a@b` loses `9000/a`. That over-redaction is accepted.
///
/// The input need not be a valid URL. The scrub is a text pass over each
/// whitespace- (ASCII or U+00A0) or quote-delimited token, so a malformed URL, or a longer
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
        if is_separator(c) || c == '"' || c == '\'' {
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
/// taken from the right, as a client would not and a leak would. A `?`
/// whose following text reaches an `@` with no `=` in between is password
/// text too (`u:pa?ss@h`), not the start of a query.
fn redact_userinfo(rest: &str, out: &mut String) {
    let query = rest.find('?');
    let limit = match (query, rest.rfind('@')) {
        (Some(q), Some(at)) if at > q && !rest[q..at].contains('=') => rest.len(),
        (Some(q), _) => q,
        (None, _) => rest.len(),
    };
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
    let mut copied = 0;
    let mut skip_to = 0;
    let mut prev_word = false;
    for (i, c) in token.char_indices() {
        if i < skip_to {
            continue;
        }
        let key_len = (!prev_word).then(|| secret_key_len(&token[i..])).flatten();
        if let Some(key_len) = key_len {
            let value_start = i + key_len + 1;
            let value_end = token[value_start..]
                .find('&')
                .map_or(token.len(), |n| value_start + n);
            out.push_str(&token[copied..value_start]);
            out.push_str(REDACTED);
            copied = value_end;
            skip_to = value_end;
            prev_word = false;
        } else {
            prev_word = is_word_char(c);
        }
    }
    out.push_str(&token[copied..]);
}

/// Length of the key at the start of `s` when it is `<key>=` and the key,
/// percent-decoded, is a secret key (the URL parser decodes key names, so
/// `pass%70hrase=x` sets the passphrase).
fn secret_key_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let len = bytes
        .iter()
        .take_while(|&&b| is_word_char(char::from(b)) || b == b'%')
        .count();
    if bytes.get(len) != Some(&b'=') {
        return None;
    }
    let mut decoded = Vec::with_capacity(len);
    let mut i = 0;
    while i < len {
        let hex = |b: u8| char::from(b).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).and_then(|&b| hex(b)),
            bytes.get(i + 2).and_then(|&b| hex(b)),
        ) {
            (b'%', Some(hi), Some(lo)) if i + 2 < len => {
                decoded.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, ..) => {
                decoded.push(b);
                i += 1;
            }
        }
    }
    SECRET_KEYS
        .iter()
        .any(|k| k.as_bytes() == decoded.as_slice())
        .then_some(len)
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Token separators: ASCII whitespace and the no-break space. The Java
/// `Redact` uses the identical set.
fn is_separator(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\u{a0}'
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
    fn non_ascii_text_does_not_panic_and_is_kept() {
        assert_eq!(redact_url("srt://h/é?latency=1"), "srt://h/é?latency=1");
        assert_eq!(
            redact_url("rtsp://u:pä\u{df}@h/é"),
            "rtsp://u:<redacted>@h/é"
        );
        assert_eq!(
            redact_url("srt://h/é?token=\u{e9}\u{e9}&x=é"),
            "srt://h/é?token=<redacted>&x=é"
        );
    }

    #[test]
    fn non_breaking_space_splits_tokens() {
        assert_eq!(
            redact_url("a\u{a0}passphrase=abc\u{a0}b"),
            "a\u{a0}passphrase=<redacted>\u{a0}b"
        );
        assert_eq!(redact_url("u:pw@h\u{a0}x"), "u:<redacted>@h\u{a0}x");
    }

    #[test]
    fn stream_id_keys_are_redacted() {
        assert_eq!(
            redact_url("srt://h:1?streamid=#!::r=x&latency=1"),
            "srt://h:1?streamid=<redacted>&latency=1"
        );
        assert_eq!(
            redact_url("srt://h:1?srt_streamid=abc"),
            "srt://h:1?srt_streamid=<redacted>"
        );
    }

    #[test]
    fn raw_question_mark_in_password_is_redacted() {
        assert_eq!(
            redact_url("rtsp://admin:pa?ss@cam/s"),
            "rtsp://admin:<redacted>@cam/s"
        );
        assert_eq!(
            redact_url("rtsp://admin:pa?ss@cam/s?token=t"),
            "rtsp://admin:<redacted>@cam/s?token=<redacted>"
        );
        // A real query holding an `@` after an `=` is not userinfo.
        assert_eq!(redact_url("srt://h:1?mail=a@b"), "srt://h:1?mail=a@b");
    }

    #[test]
    fn late_at_over_redacts_and_leading_query_key_is_scrubbed() {
        assert_eq!(redact_url("srt://h:9000/a@b"), "srt://h:<redacted>@b");
        assert_eq!(redact_url("?token=abc"), "?token=<redacted>");
    }

    #[test]
    fn several_urls_in_one_token() {
        assert_eq!(
            redact_url("a,srt://u:p@h,srt://v:q@h2"),
            "a,srt://u:<redacted>@h2"
        );
    }

    #[test]
    fn percent_encoded_keys_are_redacted() {
        assert_eq!(
            redact_url("srt://h:1?pass%70hrase=abc&x=1"),
            "srt://h:1?pass%70hrase=<redacted>&x=1"
        );
        assert_eq!(redact_url("?%74oken=abc"), "?%74oken=<redacted>");
        assert_eq!(redact_url("srt://h:1?pass%7=abc"), "srt://h:1?pass%7=abc");
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(redact_url("hello world"), "hello world");
        assert_eq!(redact_url(""), "");
        assert_eq!(redact_url("user@host"), "user@host");
    }
}
