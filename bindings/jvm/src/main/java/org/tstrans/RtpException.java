package org.tstrans;

/**
 * Checked exception for the RTP transport surface ({@code org.tstrans.rtp}).
 * Mirrors tst-py's {@code tstrans.exceptions.RtpError} / {@code RtpErrorKind}.
 * {@link Kind} maps the Rust {@code tst_core::transport::TransportError} and
 * {@code tst_rtp::ConnectError} families onto four user-facing buckets — see
 * {@code bindings/jvm/src/rtp/errors.rs}.
 */
public final class RtpException extends BindingException {
    private static final long serialVersionUID = 1L;

    /** RTP failure category. Names match tst-py {@code RtpErrorKind} 1:1. */
    public enum Kind {
        TRANSPORT, MALFORMED_PACKET, CANCELLED,
        /**
         * Recv deadline expired — retryable; the transport/session is still
         * alive. Raised from two triggers: a persistent deadline configured
         * via the {@code ?recv_timeout=<ms>} URL query key on a receiver URL,
         * or a per-call timeout argument to {@code recv()} / {@code recvAu()}.
         * A receiver with neither configured blocks indefinitely instead of
         * raising this. Mirrors tst-py {@code RtpErrorKind.TIMEOUT}.
         */
        TIMEOUT,
        /**
         * A receive deadline elapsed with the transport still alive — the
         * persistent {@code ?recv_timeout=<ms>} URL knob or the per-call
         * {@code timeoutMs} argument; retryable.
         */
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
