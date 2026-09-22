package org.tstrans;

/**
 * Checked exception for the RTP transport surface ({@code org.tstrans.rtp}).
 * {@link Kind} carries the Rust {@code tst_core::transport::TransportError}
 * projections an rtp shell can raise plus {@code tst_rtp::ConnectError}'s six
 * variants; the names are the {@code BindingErrorKind} members, verified
 * against this enum at load time by {@link NativeLoader}.
 */
public final class RtpException extends BindingException {
    private static final long serialVersionUID = 1L;

    public enum Kind {
        BACKPRESSURE,
        /** The transport is dead (send/recv I/O failure); reopen. */
        BROKEN,
        /**
         * The transport was closed — by {@code close()} on this object, or by a
         * {@link org.tstrans.rtp.CancelHandle#cancel()} / {@code close()} from
         * another thread while this call was parked (detail
         * {@code cancelled from another thread}).
         */
        CLOSED,
        /** The payload exceeds the datagram cap ({@code pktSize − 12}). */
        TOO_LARGE,
        /** {@code URL has ?pt= (elementary RTP): use H264Receiver, not the MP2T transport}. */
        PAYLOAD_TYPE_PARAM,
        /** {@code H264Receiver requires ?pt=<dynamic PT> on the URL}. */
        MISSING_PAYLOAD_TYPE_PARAM,
        /** {@code URL parse failed} — a malformed {@code rtp://} / {@code rtsp://} URL. */
        URL,
        /**
         * {@code host '<h>' is not a literal IPv4/IPv6 address} — the RTP
         * transport does no DNS resolution; pre-resolve and pass the literal.
         */
        HOST_NOT_LITERAL,
        /** {@code UDP socket error} — an OS-level bind / connect / setsockopt failure. */
        IO,
        /**
         * {@code multicast iface '<i>' unsupported} — typically the platform
         * requires a different form (IPv6 needs a scope-id integer).
         */
        IFACE_UNSUPPORTED
    }

    private final Kind kind;

    public RtpException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
