package org.tstrans.srt;

/**
 * Where the managed-reconnect SRT wrappers run their reconnect loop after the
 * inner transport breaks. Mirrors {@code tst_pipeline::ReconnectMode}. Set via
 * {@link ReconnectPolicy.Builder#mode(ReconnectMode)}; default {@link #BLOCKING}.
 */
public enum ReconnectMode {
    /** Reconnect on the caller's thread (the pre-0.6 behavior and the default). */
    BLOCKING(0),
    /**
     * Reconnect on a per-outage background worker thread. A send never waits
     * out backoff or a factory call; while the inner transport is down it
     * enqueues to the gap buffer under the configured {@link OverflowPolicy}
     * instead. Returning normally from a send means the bytes were
     * <em>accepted</em>, not <em>delivered</em>. Send-side only — a managed
     * receiver accepts this value structurally but reconnects as
     * {@link #BLOCKING} regardless (see the affected classes' javadoc).
     */
    BACKGROUND(1);

    private final int code;

    ReconnectMode(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
