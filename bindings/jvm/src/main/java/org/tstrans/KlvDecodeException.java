package org.tstrans;

/**
 * Thrown when typed-KLV decode rejects input. {@link Kind} mirrors the
 * decode-error classification in {@code tst_core::error::KlvDecodeError}.
 */
public final class KlvDecodeException extends BindingException {
    private static final long serialVersionUID = 1L;

    /**
     * Discriminant; values map 1:1 from the Rust {@code KlvDecodeError} variants.
     *
     * <p>Persist {@link #name()}, never {@link #ordinal()}: the declaration order is not part of the API.
     */
    public enum Kind {
        TRUNCATED_SET, BAD_UNIVERSAL_LABEL, CHECKSUM_MISMATCH,
        DUPLICATE_TAG, MISSING_REQUIRED_TAG, MALFORMED_BYTES, INTERNAL
    }

    private final Kind kind;

    public KlvDecodeException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
