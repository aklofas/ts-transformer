package org.tstrans.rtp;

/**
 * Why an RTP receive session ended. Mirrors {@code tst_rtp::StreamEndReason}.
 *
 * <p>Returned by {@code endReason()} on {@link Receiver}, {@link DemuxReceiver},
 * and {@link H264Receiver}; {@code null} means the session either hasn't ended
 * yet or ended through a path this type doesn't instrument (e.g. a plain
 * {@code rtp://} receiver that was never closed or cancelled).
 *
 * <p>Numeric wire values are pinned across the C, Python, and JVM bindings
 * (the C {@code TstStreamEndReason} enum additionally has {@code NONE = 0};
 * this binding uses Java {@code null} for that case instead of a member,
 * matching tst-py's convention). {@link #fromWireOrdinal} maps the native
 * ordinal to a constant via an EXPLICIT switch on those pinned values —
 * never {@code values()[ordinal]} / {@link Enum#ordinal()} — so reordering
 * this enum's declaration can never silently break the native contract.
 *
 * <p>{@code endDetail()} carries the free-text detail for
 * {@code KEEPALIVE_FAILED} / {@code TRANSPORT_FAILED} / {@code PROTOCOL_ERROR};
 * {@code null} for the other three reasons.
 */
public enum StreamEndReason {
    /** The peer closed the connection in an orderly way, with no protocol or
     *  transport error. */
    CLEAN_TEARDOWN,
    /** The server no longer honors the session — a keepalive ping was
     *  answered {@code 454 Session Not Found}. */
    SESSION_EXPIRED,
    /** The keepalive background thread failed to encode or send a ping.
     *  Detail: see {@code endDetail()}. */
    KEEPALIVE_FAILED,
    /** A hard I/O error on the underlying transport (a read failure other
     *  than clean EOF). Detail: see {@code endDetail()}. */
    TRANSPORT_FAILED,
    /** The peer violated the wire protocol and the session was failed
     *  rather than silently tolerated. Detail: see {@code endDetail()}. */
    PROTOCOL_ERROR,
    /** The caller explicitly cancelled or closed the transport/receiver —
     *  not a wire-level failure. */
    CANCELLED;

    /**
     * Map the cross-surface-pinned wire ordinal (1-6) to its constant.
     * {@code -1} — or any value this binding doesn't recognize, e.g. a
     * future non-exhaustive {@code tst_rtp::StreamEndReason} variant — maps
     * to {@code null}.
     */
    static StreamEndReason fromWireOrdinal(int ordinal) {
        switch (ordinal) {
            case 1: return CLEAN_TEARDOWN;
            case 2: return SESSION_EXPIRED;
            case 3: return KEEPALIVE_FAILED;
            case 4: return TRANSPORT_FAILED;
            case 5: return PROTOCOL_ERROR;
            case 6: return CANCELLED;
            default: return null;
        }
    }
}
