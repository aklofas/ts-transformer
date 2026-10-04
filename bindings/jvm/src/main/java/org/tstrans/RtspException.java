package org.tstrans;

/**
 * Checked exception for the RTSP control-plane surface ({@code org.tstrans.rtp}
 * RTSP client/server). Mirrors tst-py's {@code tstrans.exceptions.RtspError} /
 * {@code RtspErrorKind}. {@link Kind} collapses three Rust enums
 * ({@code tst_rtp::RtspError}, {@code RtspServerError}, {@code MountError}) onto
 * ten user-facing buckets — see {@code bindings/jvm/src/rtp/errors.rs}.
 */
public final class RtspException extends BindingException {
    private static final long serialVersionUID = 1L;

    /**
     * RTSP failure category. Names match tst-py {@code RtspErrorKind} 1:1.
     *
     * <p>Persist {@link #name()}, never {@link #ordinal()}: the declaration order is not part of the API.
     */
    public enum Kind {
        PROTOCOL, AUTH_FAILED, AUTH_REQUIRED, NOT_FOUND, UNSUPPORTED_TRANSPORT,
        TLS, IO, TIMEOUT, SERVER, MOUNT, CLOSED
    }

    private final Kind kind;

    public RtspException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
