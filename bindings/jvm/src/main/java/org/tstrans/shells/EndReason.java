package org.tstrans.shells;

/**
 * Why a receive session ended ({@code Receiver.endReason()},
 * {@code DemuxReceiver.endReason()}); {@code null} while it has not ended, or
 * when the transport records none. The wire values are the cross-surface
 * numbering shared with the C ABI's {@code TstStreamEndReason} and tst-py's
 * {@code EndReason}.
 */
public enum EndReason {
    /** The peer or this side tore the session down cleanly. */
    CLEAN_TEARDOWN(1),
    /** The session timed out on the server. */
    SESSION_EXPIRED(2),
    /** Keepalives failed; {@code endDetail()} carries the message. */
    KEEPALIVE_FAILED(3),
    /** The transport failed; {@code endDetail()} carries the message. */
    TRANSPORT_FAILED(4),
    /** The peer broke the protocol; {@code endDetail()} carries the message. */
    PROTOCOL_ERROR(5),
    /** The receiver was cancelled or closed from this side. */
    CANCELLED(6),
    /** The stream ended: the peer closed it cleanly. */
    END_OF_STREAM(7),
    /** A managed receiver ran out of reconnect attempts. */
    RECONNECT_EXHAUSTED(8);

    private final int wireValue;

    EndReason(int wireValue) { this.wireValue = wireValue; }

    /** @return the cross-surface wire value (1 to 8). */
    public int wireValue() { return wireValue; }

    /** @return the constant with this wire value, or {@code null} for anything else. */
    static EndReason fromWireValue(int v) {
        for (EndReason r : values()) {
            if (r.wireValue == v) return r;
        }
        return null;
    }

    /**
     * Package-private carrier {@code nClose} returns: the receiver's end reason
     * and detail as they stood after teardown, cached by the Java object because
     * its native handle no longer exists.
     */
    static final class Snapshot {
        final EndReason reason;
        final String detail;

        Snapshot(int wireValue, String detail) {
            this.reason = fromWireValue(wireValue);
            this.detail = detail;
        }
    }
}
