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
