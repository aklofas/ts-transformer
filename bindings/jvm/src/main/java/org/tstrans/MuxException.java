package org.tstrans;

/**
 * Thrown when the MPEG-TS muxer rejects a config or push. {@link Kind} mirrors
 * the 5-variant {@code tst_core::error::MuxErrorKind} coarse classification
 * (the same buckets tst-py's {@code MuxErrorKind} uses).
 */
public final class MuxException extends BindingException {
    private static final long serialVersionUID = 1L;

    /** Discriminant; values match the Rust {@code MuxErrorKind} variants. */
    public enum Kind {
        INPUT_MALFORMED, CONFIG_INVALID, INVALID_USAGE, BACKPRESSURE, INTERNAL,
        /** {@code MuxError::InvalidNal} — not Annex-B / not a NAL. */
        INVALID_NAL,
        /** {@code MuxError::KlvTooLarge} — KLV record over the PES ceiling. */
        KLV_TOO_LARGE,
        /** {@code MuxError::InvalidAv1Obu} — not a well-formed AV1 OBU stream. */
        INVALID_AV1_OBU,
        /** {@code MuxError::MispTime} — MISP timestamp rejected for this codec/carriage. */
        MISP_TIME
    }

    private final Kind kind;

    public MuxException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
