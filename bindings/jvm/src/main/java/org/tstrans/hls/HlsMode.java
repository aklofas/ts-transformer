package org.tstrans.hls;

/**
 * Playlist mode. Mirrors {@code tst_hls::HlsMode} / {@code tstrans.hls.HlsMode}.
 * {@link #code()} is the integer handed to the native builder.
 */
public enum HlsMode {
    /** Rolling window; segments age out of the playlist. */
    LIVE(0),
    /** Growing playlist; {@code #EXT-X-ENDLIST} written at finish. */
    EVENT(1),
    /** Complete playlist written once at finish. */
    VOD(2);

    private final int code;

    HlsMode(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
