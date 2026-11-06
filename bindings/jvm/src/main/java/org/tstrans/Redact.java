package org.tstrans;

import java.util.ArrayList;
import java.util.List;

/**
 * Removes secrets from URL text before it reaches a {@code toString()} or an exception message.
 *
 * <p><b>Implementation detail.</b> Public only so the sibling packages can call it; not part of
 * the stable API. Mirrors {@code tst_core::url::redact_url} rule for rule: the password of
 * {@code user:password@host} userinfo and the values of the query keys {@code passphrase},
 * {@code secret}, {@code token}, {@code auth_pass}, {@code password}, {@code streamid} and
 * {@code srt_streamid} become {@code <redacted>}. Key matching is exact and lower-case, after
 * percent-decoding the key name. A {@code scheme://} span is scrubbed whole. It runs to the end
 * of the input, or, when it opens right after a quote, to the last matching quote before the next
 * URL opened by the same quote, when that quote ends the text or is followed by a separator,
 * {@code ,}, {@code ;}, {@code .}, {@code :} or a closing bracket. Otherwise it runs to the end
 * of the input. Text outside any such span is scrubbed one whitespace- (ASCII or U+00A0) or
 * quote-delimited token at a time.
 *
 * <p>The scrub is a heuristic that errs toward redacting, so malformed text around a credential
 * may be redacted with it. The accepted over-redactions: any later {@code @} ends the userinfo,
 * so {@code srt://h:9000/a@b} loses {@code 9000/a}; a raw {@code ?} before the last {@code @} is
 * password text, so {@code srt://h:1?mail=a@b} loses {@code 1?mail=a}; a secret value holding an
 * {@code @} is redacted together with that userinfo, so {@code srt://u:p@h?passphrase=a@b} loses
 * everything after {@code u:}; text after a quoted URL, up to the last matching quote, is
 * treated as part of the URL, so {@code 'srt://h?token=x' or 'y'} loses {@code x' or 'y}; when
 * that last quote is followed by other text, the URL runs on to the end, so
 * {@code 'srt://h?token=x' can't} loses everything after {@code token=}, and a later quoted URL
 * in the same text is redacted as part of it.
 *
 * <p>One residual is not covered: a password or secret value that holds the quote character
 * followed by a space or by <code>, ; . : ) ] &#125;</code> is not protected. That quote reads as
 * the end of the quoted URL, so the rest of the secret stays visible ({@code 'srt://u:p' w@h} is
 * unchanged). It cannot be told apart from a well-formed quoted URL followed by text.
 */
public final class Redact {
    private static final String[] SECRET_KEYS =
        {"passphrase", "secret", "token", "auth_pass", "password", "streamid", "srt_streamid"};
    /**
     * The longest a secret key can be on the wire: every character percent-encoded. A longer run
     * of key characters decodes to more characters than any secret key holds.
     */
    private static final int MAX_ENCODED_KEY_LEN =
        3 * java.util.Arrays.stream(SECRET_KEYS).mapToInt(String::length).max().getAsInt();
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
            // A quote or a space can sit inside a password or a value, so a quoted URL runs to the
            // last matching quote, not the first.
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
     * Where a span opened by quote {@code q} ends: the last {@code q} at or after {@code from}
     * and before the next URL that is itself opened by {@code q}, not counting that opening
     * {@code q}, when that {@code q} ends the text or is followed by a
     * {@link #followsClosingQuote} character; the end of the input otherwise. Stopping there
     * keeps a second quoted URL its own span, while a bare scheme-looking word inside the quoted
     * text (a password holding {@code y://}) does not end it. A {@code q} opens that next URL
     * only at the start of the text or after a character {@link #precedesOpeningQuote} accepts,
     * so a secret holding {@code 'srt://} does not end the span either.
     */
    private static int closingQuote(String s, int from, char q) {
        int limit = s.length();
        for (int[] next = nextUrl(s, from); next != null; next = nextUrl(s, next[1])) {
            int quote = next[0] - 1;
            if (quote >= 0 && s.charAt(quote) == q
                    && (quote == 0 || precedesOpeningQuote(s.charAt(quote - 1)))) {
                limit = quote;
                break;
            }
        }
        int i = s.lastIndexOf(q, limit - 1);
        if (i < from) {
            return s.length();
        }
        // A quote followed by more text ('srt://u:p'w@h) sits inside a password or a value;
        // closing there would leave the rest visible. With no quote that closes the span, the URL
        // that set the limit may be part of the secret too (abc'def='srt://x'), so the span runs
        // to the end.
        if (i + 1 == limit || i + 1 == s.length() || followsClosingQuote(s.charAt(i + 1))) {
            return i;
        }
        return s.length();
    }

    /**
     * Characters that may follow the quote that closes a quoted URL: a token separator,
     * {@code ,}, {@code ;}, {@code .}, {@code :} or a closing bracket; identical to the Rust
     * helper.
     */
    private static boolean followsClosingQuote(char c) {
        return isSeparator(c) || c == ',' || c == ';' || c == '.' || c == ':'
            || c == ')' || c == ']' || c == '}';
    }

    /**
     * Characters after which a quote may open a quoted URL: a token separator, {@code ,},
     * {@code ;}, {@code :}, {@code =} or an opening bracket; identical to the Rust helper.
     */
    private static boolean precedesOpeningQuote(char c) {
        return isSeparator(c) || c == ',' || c == ';' || c == ':' || c == '='
            || c == '(' || c == '[' || c == '{';
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
        int rest = scheme < 0 ? 0 : scheme + 3;
        // Both masks come from the original text: a secret value holding an @ (?passphrase=a@b)
        // also reads as userinfo, and masking the userinfo first would hide the key and leave
        // the value's tail visible.
        List<int[]> masks = secretValueRanges(token, rest);
        int[] userinfo = userinfoMask(token.substring(rest));
        if (userinfo != null) {
            masks.add(new int[] {rest + userinfo[0], rest + userinfo[1]});
        }
        masks.sort((a, b) -> a[0] != b[0] ? Integer.compare(a[0], b[0])
            : Integer.compare(a[1], b[1]));
        int copied = 0;
        for (int m = 0; m < masks.size(); m++) {
            int from = masks.get(m)[0];
            int to = masks.get(m)[1];
            while (m + 1 < masks.size() && masks.get(m + 1)[0] <= to) {
                to = Math.max(to, masks.get(m + 1)[1]);
                m++;
            }
            out.append(token, copied, from).append(REDACTED);
            copied = to;
        }
        out.append(token, copied, token.length());
    }

    /**
     * The password in the userinfo that precedes the last {@code @}, as {@code {from, to}} offsets
     * of {@code rest}, or null. It may contain {@code /}, {@code @} or a raw {@code ?}
     * ({@code u:pa?ss=w@h}). A {@code ?} with no {@code @} after it starts the query.
     */
    private static int[] userinfoMask(String rest) {
        int q = rest.indexOf('?');
        int lastAt = rest.lastIndexOf('@');
        int limit = q < 0 || lastAt > q ? rest.length() : q;
        int at = rest.lastIndexOf('@', limit - 1);
        int colon = at < 0 ? -1 : rest.indexOf(':');
        return at >= 0 && colon >= 0 && colon < at ? new int[] {colon + 1, at} : null;
    }

    /**
     * Offsets of the values of secret {@code key=value} pairs in {@code token} at or after
     * {@code from}. A value runs to the next {@code &} or the end of the token.
     */
    private static List<int[]> secretValueRanges(String token, int from) {
        List<int[]> ranges = new ArrayList<>();
        int i = from;
        while (i < token.length()) {
            int keyLen = (i == 0 || !isWord(token.charAt(i - 1))) ? secretKeyLen(token, i) : -1;
            if (keyLen >= 0) {
                int valueStart = i + keyLen + 1;
                int amp = token.indexOf('&', valueStart);
                int valueEnd = amp < 0 ? token.length() : amp;
                ranges.add(new int[] {valueStart, valueEnd});
                i = valueEnd;
            } else {
                i++;
            }
        }
        return ranges;
    }

    /**
     * Length of the key at {@code i} when it is {@code <key>=} and the key, percent-decoded, is a
     * secret key (the URL parser decodes key names, so {@code pass%70hrase=x} sets the passphrase);
     * -1 otherwise.
     */
    private static int secretKeyLen(String token, int i) {
        // A longer run cannot be a secret key, so the scan stops one character past the longest
        // encoded one; scanning the whole run at every position costs time quadratic in its length.
        int cap = Math.min(token.length(), i + MAX_ENCODED_KEY_LEN + 1);
        int end = i;
        while (end < cap && (isWord(token.charAt(end)) || token.charAt(end) == '%')) {
            end++;
        }
        if (end - i > MAX_ENCODED_KEY_LEN || end >= token.length() || token.charAt(end) != '=') {
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
