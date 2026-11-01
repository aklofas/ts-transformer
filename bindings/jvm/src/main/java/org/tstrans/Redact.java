package org.tstrans;

/**
 * Removes secrets from URL text before it reaches a {@code toString()} or an exception message.
 *
 * <p><b>Implementation detail.</b> Public only so the sibling packages can call it; not part of
 * the stable API. Mirrors {@code tst_core::url::redact_url} rule for rule: the password of
 * {@code user:password@host} userinfo and the values of the query keys {@code passphrase},
 * {@code secret}, {@code token}, {@code auth_pass} and {@code password} become
 * {@code <redacted>}. Key matching is exact and lower-case. The scrub is a text pass over each
 * whitespace- or quote-delimited token, so a malformed URL, or a message that embeds one, never
 * leaks either kind of secret.
 */
public final class Redact {
    private static final String[] SECRET_KEYS =
        {"passphrase", "secret", "token", "auth_pass", "password"};
    private static final String REDACTED = "<redacted>";

    private Redact() {}

    /** @return {@code url} with its secrets replaced by {@code <redacted>}. */
    public static String url(String url) {
        StringBuilder out = new StringBuilder(url.length());
        int start = 0;
        for (int i = 0; i < url.length(); i++) {
            char c = url.charAt(i);
            if (Character.isWhitespace(c) || c == '"' || c == '\'') {
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

    /** The password sits before the last {@code @} that precedes any {@code ?}; it may contain {@code /} or {@code @}. */
    private static void userinfo(String rest, StringBuilder out) {
        int q = rest.indexOf('?');
        int limit = q < 0 ? rest.length() : q;
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
            String key = (i == 0 || !isWord(token.charAt(i - 1))) ? keyAt(token, i) : null;
            if (key != null) {
                int valueStart = i + key.length() + 1;
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

    private static String keyAt(String token, int i) {
        for (String k : SECRET_KEYS) {
            if (token.startsWith(k + "=", i)) {
                return k;
            }
        }
        return null;
    }

    private static boolean isWord(char c) {
        return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
            || c == '_' || c == '-';
    }
}
