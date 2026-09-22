package org.tstrans;

/**
 * Checked exception for the SRT transport surface ({@code org.tstrans.srt}).
 * Mirrors tst-py's {@code tstrans.exceptions.SrtError} / {@code SrtErrorKind}.
 * {@link Kind} maps the Rust {@code tst_srt} error families
 * (UrlError / ConnectError / BindError / AcceptError / IoError / TransportError)
 * onto twelve user-facing buckets — see {@code bindings/jvm/src/srt/errors.rs}.
 */
public final class SrtException extends BindingException {
    private static final long serialVersionUID = 1L;

    /**
     * SRT failure category. Names are the Rust {@code BindingErrorKind} variant
     * names in SCREAMING_SNAKE_CASE (the kind rule, 0.7.0); the native library
     * verifies at load time that every kind it can raise resolves here.
     */
    public enum Kind {
        CONFIG_INVALID, CONNECT_FAILED, ACCEPT_FAILED, TIMEOUT,
        CLOSED, BROKEN, IO,
        /** The transport is alive but could not take the bytes now (full send queue); retry. */
        BACKPRESSURE,
        /** The payload exceeds the transport's per-send ceiling. */
        TOO_LARGE,
        /** {@code sendBytes} input lost MPEG-TS sync (not 188-byte-aligned packets). */
        INPUT_MALFORMED,
        /**
         * The receive loop reached end of stream. The receiver is dead; later
         * calls report {@code CLOSED}. Same kind the C ABI returns as
         * {@code TST_E_END_OF_STREAM} (-12).
         *
         * <p>Two producers:
         * <ul>
         *   <li>a peer closes the session cleanly — a sender opened with the
         *       sender preset ({@code SRTO_SENDER} + linger), e.g. via the C
         *       ABI or an {@code org.tstrans.srt.ManagedSender}; the plain JVM
         *       caller shells do not set that preset, so a JVM-to-JVM plain
         *       loopback reports {@code BROKEN} instead;</li>
         *   <li>an {@code org.tstrans.srt.ManagedReceiver} whose reconnect
         *       budget is exhausted — the decorator gives up and the receive
         *       direction classifies that as end of stream.</li>
         * </ul>
         *
         * <p>The message carries the transport's own text, which differs
         * between the two. Before 0.7.0 both surfaced as {@code CLOSED},
         * indistinguishable from a locally-closed transport.
         */
        END_OF_STREAM
    }

    private final Kind kind;

    public SrtException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
