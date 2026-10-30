package org.tstrans.srt;

import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Optional;
import org.tstrans.NativeLoader;

/**
 * The key-value pairs of libsrt's recommended {@code #!::key=value,...}
 * stream-ID form, for example {@code #!::u=admin,r=stream1}.
 *
 * <p>Values are taken as given: the form has no escaping, so a value cannot
 * contain a comma. {@link #parse(String)} refuses a string without the
 * {@code #!} prefix, the nested {@code #!{...}} form, malformed pairs and a
 * repeated key, each with an {@link IllegalArgumentException}.
 */
public final class AccessControl {

    static { NativeLoader.load(); }

    /** The {@code t} key: what the caller wants to transfer. */
    public enum Kind { STREAM, FILE, AUTH, OTHER }

    /** The {@code m} key: the direction of the transfer. */
    public enum Mode { REQUEST, PUBLISH, BIDIRECTIONAL, OTHER }

    private final Map<String, String> pairs;

    private AccessControl(Map<String, String> pairs) {
        this.pairs = Collections.unmodifiableMap(pairs);
    }

    private static native String[] nParse(String streamId);

    /**
     * Parses a {@code #!::key=value,...} stream ID.
     *
     * @throws IllegalArgumentException if {@code streamId} is not that form
     */
    public static AccessControl parse(String streamId) {
        if (streamId == null) {
            throw new IllegalArgumentException("streamId is null");
        }
        String[] flat = nParse(streamId);
        Map<String, String> m = new LinkedHashMap<>();
        for (int i = 0; i + 1 < flat.length; i += 2) {
            m.put(flat[i], flat[i + 1]);
        }
        return new AccessControl(m);
    }

    /** The {@code u} key: the user. */
    public Optional<String> user() { return get("u"); }

    /** The {@code r} key: the resource name. */
    public Optional<String> resource() { return get("r"); }

    /** The {@code h} key: the host the resource is on. */
    public Optional<String> host() { return get("h"); }

    /** The {@code s} key: the session identifier. */
    public Optional<String> session() { return get("s"); }

    /** The {@code t} key as text; {@code "stream"} when absent. */
    public String kindRaw() { return pairs.getOrDefault("t", "stream"); }

    /** The {@code t} key; {@link Kind#OTHER} for a value libsrt does not define. */
    public Kind kind() {
        switch (kindRaw()) {
            case "stream": return Kind.STREAM;
            case "file": return Kind.FILE;
            case "auth": return Kind.AUTH;
            default: return Kind.OTHER;
        }
    }

    /** The {@code m} key as text; {@code "request"} when absent. */
    public String modeRaw() { return pairs.getOrDefault("m", "request"); }

    /** The {@code m} key; {@link Mode#OTHER} for a value libsrt does not define. */
    public Mode mode() {
        switch (modeRaw()) {
            case "request": return Mode.REQUEST;
            case "publish": return Mode.PUBLISH;
            case "bidirectional": return Mode.BIDIRECTIONAL;
            default: return Mode.OTHER;
        }
    }

    /** The value of {@code key}, custom keys included. */
    public Optional<String> get(String key) { return Optional.ofNullable(pairs.get(key)); }

    /** Every pair in the order the stream ID gave them; unmodifiable. */
    public Map<String, String> all() { return pairs; }
}
