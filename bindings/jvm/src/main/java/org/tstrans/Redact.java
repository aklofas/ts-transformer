package org.tstrans;

/**
 * Removes secrets from URL text before it reaches a {@code toString()} or an exception message.
 *
 * <p><b>Implementation detail.</b> Public only so the sibling packages can call it; not part of
 * the stable API. Mirrors {@code tst_core::url::redact_url} rule for rule: the password of
 * {@code user:password@host} userinfo and the values of the query keys {@code passphrase},
 * {@code secret}, {@code token}, {@code auth_pass}, {@code password}, {@code streamid} and
 * {@code srt_streamid} become {@code <redacted>}. Key matching is exact and lower-case, after
 * percent-decoding the key name. A {@code scheme://} span runs to the end of the input, or to the
 * closing quote when it opens right after one, and is scrubbed whole: a quote or a space inside a
 * password or a value does not end it. Text outside any such span is scrubbed one whitespace-
 * (ASCII or U+00A0) or quote-delimited token at a time, so a malformed URL, or a message that
 * embeds one, never leaks either kind of secret. It errs toward
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
        int pos = 0;
        while (true) {
            int[] span = nextUrl(url, pos);
            if (span == null) {
                break;
            }
            int start = span[0];
            tokens(url.substring(pos, start), out);
            // A URL runs to the end of the input, or to the closing quote when it opens right
            // after one: quotes and spaces inside it are part of a password or a value.
            int end = url.length();
            if (start > 0) {
                char q = url.charAt(start - 1);
                if (q == '"' || q == '\'') {
                    end = closingQuote(url, span[1], q);
                }
            }
            token(url.substring(start, end), out);
            pos = end;
        }
        tokens(url.substring(pos), out);
        return out.toString();
    }

    /**
     * Where a span opened by quote {@code q} ends: the first {@code q} at or after {@code from}
     * followed by a separator, a closing bracket, {@code ,} or {@code ;}, or by the end of the
     * input. A {@code q} inside a password or a value does not close it.
     */
    private static int closingQuote(String s, int from, char q) {
        for (int i = s.indexOf(q, from); i >= 0; i = s.indexOf(q, i + 1)) {
            if (i + 1 == s.length()) {
                return i;
            }
            char c = s.charAt(i + 1);
            if (isSeparator(c) || c == ',' || c == ';' || c == ')' || c == ']' || c == '}') {
                return i;
            }
        }
        return s.length();
    }

    /** Offsets of the next {@code scheme://} at or after {@code from}: scheme start, text after. */
    private static int[] nextUrl(String s, int from) {
        int search = from;
        while (true) {
            int sep = s.indexOf("://", search);
            if (sep < 0) {
                return null;
            }
            int start = sep;
            while (start > from && isSchemeChar(s.charAt(start - 1))) {
                start--;
            }
            if (start < sep) {
                return new int[] {start, sep + 3};
            }
            search = sep + 3;
        }
    }

    private static boolean isSchemeChar(char c) {
        return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
            || c == '+' || c == '.' || c == '-';
    }

    /** Free text outside any URL: scrubbed one separator-delimited token at a time. */
    private static void tokens(String text, StringBuilder out) {
        int start = 0;
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (isSeparator(c) || c == '"' || c == '\'') {
                token(text.substring(start, i), out);
                out.append(c);
                start = i + 1;
            }
        }
        token(text.substring(start), out);
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
