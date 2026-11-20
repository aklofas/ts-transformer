package org.tstrans.srt;

/**
 * What a managed transport does when the gap buffer is full and a new message
 * arrives during an outage. Mirrors {@code tstrans.srt.OverflowPolicy} /
 * {@code tst_pipeline::reconnect::gap_buffer::OverflowPolicy}.
 */
public enum OverflowPolicy {
    /** Evict the front of the queue to make room (the default). */
    DROP_OLDEST(0),
    /** Refuse to enqueue; surface an error to the caller. */
    REJECT(1);

    private final int code;

    OverflowPolicy(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
