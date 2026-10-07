package org.tstrans;

/**
 * Checked exception for the HLS publisher surface ({@code org.tstrans.hls}).
 * Mirrors tst-py's {@code tstrans.exceptions.HlsError} / {@code HlsErrorKind}.
 * {@link Kind} names are the Rust {@code BindingErrorKind} HLS variants with the
 * {@code HLS_} prefix stripped; the native library verifies at load time that
 * every kind it can raise resolves here (see {@code bindings/jvm/src/error.rs}).
 */
public final class HlsException extends BindingException {
    private static final long serialVersionUID = 1L;

    /**
     * HLS failure category. Persist {@link #name()}, never {@link #ordinal()}.
     */
    public enum Kind {
        /** Segment / playlist file I/O failed. */
        IO,
        /** The builder's configuration was rejected at {@code build()}. */
        INVALID_CONFIG,
        /** The Rust publisher was already finished (library-raised; a closed JVM handle throws {@code IllegalStateException} instead). */
        FINISHED,
        /** TLS setup failed (unreadable cert/key, handshake). */
        TLS,
        /** {@code fromUrl} could not parse the URL. */
        URL,
        /** The built-in HTTP server could not bind. */
        BIND_FAILED,
        /** {@code pushTs} input is not a whole multiple of 188 bytes. */
        UNALIGNED_PUSH_TS,
        /** {@code enableTls} on a build without the {@code tls} feature. */
        TLS_DISABLED,
        /** The {@code MuxPublisher} shell was already consumed. */
        CLOSED,
        /** A poisoned lock or other binding-internal failure. */
        INTERNAL
    }

    private final Kind kind;

    public HlsException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
