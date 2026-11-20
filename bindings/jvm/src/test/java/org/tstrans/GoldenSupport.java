package org.tstrans;

/**
 * Field readers for the committed scenario {@code golden.json} files. The test classpath has no
 * JSON library, so these locate a key by text. A missing key throws {@link AssertionError} naming
 * it, so a golden that lost a field fails the test instead of letting it pass without comparing.
 */
public final class GoldenSupport {

    private GoldenSupport() {}

    /** The {@code {...}} text of the core object whose event is {@code "video"}. */
    public static String videoObject(String json) {
        return eventObject(json, "video");
    }

    /** The {@code {...}} text of the core object carrying the {@code "<event>"} marker. */
    public static String eventObject(String json, String event) {
        int marker = json.indexOf("\"" + event + "\"");
        if (marker < 0) {
            throw new AssertionError("golden has no \"" + event + "\" core event: " + json);
        }
        int start = json.lastIndexOf('{', marker);
        int end = json.indexOf('}', marker);
        if (start < 0 || end <= start) {
            throw new AssertionError("could not bound the \"" + event + "\" core object in golden");
        }
        return json.substring(start, end + 1);
    }

    /** The {@code {...}} text of the flat object under {@code "key"}. */
    public static String objectField(String json, String key) {
        return bracketed(json, key, '{', '}');
    }

    /** The {@code [...]} text of the array under {@code "key"}; its elements are flat objects. */
    public static String arrayField(String json, String key) {
        return bracketed(json, key, '[', ']');
    }

    private static String bracketed(String json, String key, char open, char close) {
        int colon = colonAfter(json, key);
        int start = json.indexOf(open, colon);
        int end = json.indexOf(close, start + 1);
        if (start < 0 || end < 0 || !json.substring(colon + 1, start).isBlank()) {
            throw new AssertionError("golden field \"" + key + "\" is not a " + open + "..." + close);
        }
        return json.substring(start, end + 1);
    }

    /** Read a string-valued field {@code "key": "<value>"}. */
    public static String stringField(String obj, String key) {
        int colon = colonAfter(obj, key);
        int first = obj.indexOf('"', colon + 1);
        if (first < 0) throw new AssertionError("golden field \"" + key + "\" is not a string");
        int last = obj.indexOf('"', first + 1);
        if (last <= first) {
            throw new AssertionError("unterminated golden string field \"" + key + "\"");
        }
        return obj.substring(first + 1, last);
    }

    /** Read a number that fits a signed 64-bit value. */
    public static long signedNumber(String obj, String key) {
        return Long.parseLong(digits(obj, key));
    }

    /** Read a number that may use the full unsigned 64-bit range. */
    public static long unsignedNumber(String obj, String key) {
        return Long.parseUnsignedLong(digits(obj, key));
    }

    private static int colonAfter(String obj, String key) {
        String needle = "\"" + key + "\"";
        int k = obj.indexOf(needle);
        if (k < 0) throw new AssertionError("golden object missing field \"" + key + "\": " + obj);
        int colon = obj.indexOf(':', k + needle.length());
        if (colon < 0) throw new AssertionError("malformed golden field \"" + key + "\"");
        return colon;
    }

    private static String digits(String obj, String key) {
        int i = colonAfter(obj, key) + 1;
        while (i < obj.length() && Character.isWhitespace(obj.charAt(i))) i++;
        int start = i;
        while (i < obj.length() && (Character.isDigit(obj.charAt(i)) || obj.charAt(i) == '-')) i++;
        if (i == start) throw new AssertionError("golden field \"" + key + "\" is not a number");
        return obj.substring(start, i);
    }
}
