package org.tstrans.hls;

/**
 * Playlist mode. Mirrors {@code tst_hls::HlsMode} / {@code tstrans.hls.HlsMode}.
 * The ordinal is the integer handed to the native builder — keep the order.
 */
public enum HlsMode {
    /** Rolling window; segments age out of the playlist. */
    LIVE,
    /** Growing playlist; {@code #EXT-X-ENDLIST} written at finish. */
    EVENT,
    /** Complete playlist written once at finish. */
    VOD
}
