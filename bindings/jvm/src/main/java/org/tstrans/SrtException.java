package org.tstrans;

/**
 * Checked exception for the SRT transport surface ({@code org.tstrans.srt}).
 * Mirrors tst-py's {@code tstrans.exceptions.SrtError} / {@code SrtErrorKind}.
 * {@link Kind} maps the Rust {@code tst_srt} error families
 * (UrlError / ConnectError / BindError / AcceptError / IoError / TransportError)
 * onto eight user-facing buckets — see {@code bindings/jvm/src/srt/errors.rs}.
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
        CLOSED, BROKEN, WOULD_BLOCK, IO,
        /** The transport is alive but could not take the bytes now (full send queue); retry. */
        BACKPRESSURE,
        /** The payload exceeds the transport's per-send ceiling. */
        TOO_LARGE,
        /** {@code sendBytes} input lost MPEG-TS sync (not 188-byte-aligned packets). */
        INPUT_MALFORMED
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
