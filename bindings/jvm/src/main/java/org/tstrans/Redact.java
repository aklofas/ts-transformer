package org.tstrans;

/**
 * Removes secrets from URL text before it reaches a {@code toString()} or an exception message.
 *
 * <p><b>Implementation detail.</b> Public only so the sibling packages can call it; not part of
 * the stable API. Mirrors {@code tst_core::url::redact_url} rule for rule: the password of
 * {@code user:password@host} userinfo and the values of the query keys {@code passphrase},
 * {@code secret}, {@code token}, {@code auth_pass}, {@code password}, {@code streamid} and
 * {@code srt_streamid} become {@code <redacted>}. Key matching is exact and lower-case, after percent-decoding the key name. The scrub
 * is a text pass over each whitespace- (ASCII or U+00A0) or quote-delimited token, so a malformed
 * URL, or a message that embeds one, never leaks either kind of secret. It errs toward
 * redacting: any later {@code @} ends the userinfo, so {@code srt://h:9000/a@b} loses
 * {@code 9000/a}.
 */
public final class Redact {
    private static final String[] SECRET_KEYS =
        {"passphrase", "secret", "token", "auth_pass", "password", "streamid", "srt_streamid"};
    private static final String REDACTED = "<redacted>";

    private Redact() {}

    /** @return {@code url} with its secrets replaced by {@code <redacted>}. */
    public static String url(String url) {
        StringBuilder out = new StringBuilder(url.length());
        int start = 0;
        for (int i = 0; i < url.length(); i++) {
            char c = url.charAt(i);
            if (isSeparator(c) || c == '"' || c == '\'') {
                token(url.substring(start, i), out);
                out.append(c);
                start = i + 1;
            }
        }
        token(url.substring(start), out);
        return out.toString();
    }

    private static void token(String token, StringBuilder out) {
        int scheme = token.indexOf("://");
        String head = scheme < 0 ? "" : token.substring(0, scheme + 3);
        String rest = scheme < 0 ? token : token.substring(scheme + 3);
        StringBuilder scrubbed = new StringBuilder(token.length());
        scrubbed.append(head);
        userinfo(rest, scrubbed);
        queryValues(scrubbed.toString(), out);
    }

    /**
     * The password sits before the last {@code @} that precedes any {@code ?}; it may contain
     * {@code /} or {@code @}. A {@code ?} whose following text reaches an {@code @} with no
     * {@code =} in between is password text too ({@code u:pa?ss@h}).
     */
    private static void userinfo(String rest, StringBuilder out) {
        int q = rest.indexOf('?');
        int lastAt = rest.lastIndexOf('@');
        int limit = q < 0 ? rest.length() : q;
        if (q >= 0 && lastAt > q) {
            int eq = rest.indexOf('=', q);
            if (eq < 0 || eq > lastAt) {
                limit = rest.length();
            }
        }
        int at = rest.lastIndexOf('@', limit - 1);
        int colon = at < 0 ? -1 : rest.indexOf(':');
        if (at >= 0 && colon >= 0 && colon < at) {
            out.append(rest, 0, colon + 1).append(REDACTED).append(rest, at, rest.length());
        } else {
            out.append(rest);
        }
    }

    private static void queryValues(String token, StringBuilder out) {
        int i = 0;
        int copied = 0;
        while (i < token.length()) {
            int keyLen = (i == 0 || !isWord(token.charAt(i - 1))) ? secretKeyLen(token, i) : -1;
            if (keyLen >= 0) {
                int valueStart = i + keyLen + 1;
                int amp = token.indexOf('&', valueStart);
                int valueEnd = amp < 0 ? token.length() : amp;
                out.append(token, copied, valueStart).append(REDACTED);
                copied = valueEnd;
                i = valueEnd;
            } else {
                i++;
            }
        }
        out.append(token, copied, token.length());
    }

    /**
     * Length of the key at {@code i} when it is {@code <key>=} and the key, percent-decoded, is a
     * secret key (the URL parser decodes key names, so {@code pass%70hrase=x} sets the passphrase);
     * -1 otherwise.
     */
    private static int secretKeyLen(String token, int i) {
        int end = i;
        while (end < token.length() && (isWord(token.charAt(end)) || token.charAt(end) == '%')) {
            end++;
        }
        if (end >= token.length() || token.charAt(end) != '=') {
            return -1;
        }
        StringBuilder decoded = new StringBuilder(end - i);
        for (int j = i; j < end; ) {
            int hi = j + 2 < end && token.charAt(j) == '%' ? Character.digit(token.charAt(j + 1), 16) : -1;
            int lo = hi < 0 ? -1 : Character.digit(token.charAt(j + 2), 16);
            if (hi >= 0 && lo >= 0) {
                decoded.append((char) (hi * 16 + lo));
                j += 3;
            } else {
                decoded.append(token.charAt(j));
                j++;
            }
        }
        return java.util.Arrays.asList(SECRET_KEYS).contains(decoded.toString()) ? end - i : -1;
    }

    /** Token separators: ASCII whitespace and the no-break space; identical to the Rust helper. */
    private static boolean isSeparator(char c) {
        return c == ' ' || c == '\t' || c == '\n' || c == '\f' || c == '\r' || c == '\u00a0';
    }

    private static boolean isWord(char c) {
        return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
            || c == '_' || c == '-';
    }
}
