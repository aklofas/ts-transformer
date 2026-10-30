package org.tstrans.srt;

import org.tstrans.NativeLoader;

/**
 * Reason code a listener sends a caller it refuses during the SRT handshake,
 * set through {@link ListenRule#reject(RejectReason)}.
 *
 * <p>Not an enum: the user-defined range ({@code 2000} and up) is open. Two
 * reasons are equal when their codes are equal. The constants cover libsrt's
 * predefined range ({@code 1000..1999}). libsrt-internal codes (below
 * {@code 1000}) have no constants here: they are outcomes a caller reads on a
 * failed connect, never something a rule may send.
 */
public final class RejectReason {

    static { NativeLoader.load(); }

    /** {@code SRT_REJX_FALLBACK} (1000): generic predefined fallback. */
    public static final RejectReason FALLBACK = new RejectReason(1000, "FALLBACK");
    /** {@code SRT_REJX_KEY_NOTSUP} (1001): stream-ID key not supported by the service. */
    public static final RejectReason KEY_NOT_SUPPORTED = new RejectReason(1001, "KEY_NOT_SUPPORTED");
    /** {@code SRT_REJX_FILEPATH} (1002): bad file path syntax, or not found. */
    public static final RejectReason FILEPATH = new RejectReason(1002, "FILEPATH");
    /** {@code SRT_REJX_HOSTNOTFOUND} (1003): host specification not recognized. */
    public static final RejectReason HOST_NOT_FOUND = new RejectReason(1003, "HOST_NOT_FOUND");
    /** {@code SRT_REJX_BAD_REQUEST} (1400): general stream-ID syntax error. */
    public static final RejectReason BAD_REQUEST = new RejectReason(1400, "BAD_REQUEST");
    /** {@code SRT_REJX_UNAUTHORIZED} (1401): authentication failed. */
    public static final RejectReason UNAUTHORIZED = new RejectReason(1401, "UNAUTHORIZED");
    /** {@code SRT_REJX_OVERLOAD} (1402): server overloaded or credits exceeded. */
    public static final RejectReason OVERLOAD = new RejectReason(1402, "OVERLOAD");
    /** {@code SRT_REJX_FORBIDDEN} (1403): access denied. */
    public static final RejectReason FORBIDDEN = new RejectReason(1403, "FORBIDDEN");
    /** {@code SRT_REJX_NOTFOUND} (1404): resource not found. */
    public static final RejectReason NOT_FOUND = new RejectReason(1404, "NOT_FOUND");
    /** {@code SRT_REJX_BAD_MODE} (1405): mode in the {@code m} key not supported. */
    public static final RejectReason BAD_MODE = new RejectReason(1405, "BAD_MODE");
    /** {@code SRT_REJX_UNACCEPTABLE} (1406): requested parameters cannot be satisfied. */
    public static final RejectReason UNACCEPTABLE = new RejectReason(1406, "UNACCEPTABLE");
    /** {@code SRT_REJX_CONFLICT} (1409): resource locked for modification. */
    public static final RejectReason CONFLICT = new RejectReason(1409, "CONFLICT");
    /** {@code SRT_REJX_NOTSUP_MEDIA} (1415): media type not supported. */
    public static final RejectReason NOT_SUPPORTED_MEDIA = new RejectReason(1415, "NOT_SUPPORTED_MEDIA");
    /** {@code SRT_REJX_LOCKED} (1423): resource locked for any access. */
    public static final RejectReason LOCKED = new RejectReason(1423, "LOCKED");
    /** {@code SRT_REJX_FAILED_DEPEND} (1424): dependent session disconnected. */
    public static final RejectReason FAILED_DEPENDENCY = new RejectReason(1424, "FAILED_DEPENDENCY");
    /** {@code SRT_REJX_ISE} (1500): unexpected internal server error. */
    public static final RejectReason INTERNAL_SERVER_ERROR = new RejectReason(1500, "INTERNAL_SERVER_ERROR");
    /** {@code SRT_REJX_UNIMPLEMENTED} (1501): the service version does not support the request. */
    public static final RejectReason UNIMPLEMENTED = new RejectReason(1501, "UNIMPLEMENTED");
    /** {@code SRT_REJX_GW} (1502): the gateway target rejected the connection. */
    public static final RejectReason GATEWAY = new RejectReason(1502, "GATEWAY");
    /** {@code SRT_REJX_DOWN} (1503): service temporarily down. */
    public static final RejectReason DOWN = new RejectReason(1503, "DOWN");
    /** {@code SRT_REJX_VERSION} (1505): SRT version not supported by the service. */
    public static final RejectReason VERSION_UNSUPPORTED = new RejectReason(1505, "VERSION_UNSUPPORTED");
    /** {@code SRT_REJX_NOROOM} (1507): out of storage to archive the stream. */
    public static final RejectReason NO_ROOM = new RejectReason(1507, "NO_ROOM");

    private static final RejectReason[] PREDEFINED = {
        FALLBACK, KEY_NOT_SUPPORTED, FILEPATH, HOST_NOT_FOUND, BAD_REQUEST, UNAUTHORIZED,
        OVERLOAD, FORBIDDEN, NOT_FOUND, BAD_MODE, UNACCEPTABLE, CONFLICT, NOT_SUPPORTED_MEDIA,
        LOCKED, FAILED_DEPENDENCY, INTERNAL_SERVER_ERROR, UNIMPLEMENTED, GATEWAY, DOWN,
        VERSION_UNSUPPORTED, NO_ROOM,
    };

    private final int code;
    private final String name;

    private RejectReason(int code, String name) {
        this.code = code;
        this.name = name;
    }

    /**
     * An application-defined reason.
     *
     * @param code at least {@code 2000} ({@code SRT_REJC_USERDEFINED})
     * @throws IllegalArgumentException if {@code code < 2000}
     */
    public static RejectReason userDefined(int code) {
        if (code < 2000) {
            throw new IllegalArgumentException(
                "a user-defined reject reason is 2000 or above, got " + code);
        }
        return new RejectReason(code, "OTHER");
    }

    /**
     * The reason for any code: the matching constant for a predefined code,
     * otherwise a reason whose {@link #name()} is {@code "OTHER"}.
     */
    public static RejectReason ofCode(int code) {
        for (RejectReason r : PREDEFINED) {
            if (r.code == code) return r;
        }
        return new RejectReason(code, "OTHER");
    }

    /** The numeric reject code carried in the handshake. */
    public int code() { return code; }

    /** The constant's name, or {@code "OTHER"} for a code without one. */
    public String name() { return name; }

    @Override public boolean equals(Object o) {
        return o instanceof RejectReason && ((RejectReason) o).code == code;
    }

    @Override public int hashCode() { return Integer.hashCode(code); }

    @Override public String toString() { return name + "(" + code + ")"; }

    /** {@code "CODE NAME"} for every typed predefined reason the native side knows. */
    static native String[] nTable();
}
