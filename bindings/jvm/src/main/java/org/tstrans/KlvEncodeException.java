package org.tstrans;

import java.util.Optional;

/**
 * Thrown when typed-KLV encode fails. {@link Kind} mirrors
 * {@code tst_core::error::KlvEncodeError}; {@link #tag()} carries the
 * offending KLV tag for most tag-bearing variants, or the VTarget Pack
 * {@code target_id} for {@code V_TARGET_PACK_EMPTY} / {@code DUPLICATE_TARGET_ID}.
 */
public final class KlvEncodeException extends BindingException {
    private static final long serialVersionUID = 1L;

    /** Discriminant; values map from the Rust {@code KlvEncodeError} variants. */
    public enum Kind {
        BUFFER_TOO_SMALL, RECORD_TOO_LARGE, OUT_OF_RANGE, STRING_TOO_LONG,
        UNSUPPORTED_IMAPB_LENGTH, INVALID_IMAPB_PARAMS,
        MISSING_MANDATORY_ITEM, RESERVED_TAG_IN_UNKNOWN,
        DUPLICATE_TARGET_ID, FORBIDDEN_STANDALONE_OFFSET,
        /**
         * {@code KlvEncodeError::VTargetPackEmpty} — an ST 0903 VTarget Pack
         * carried no items; {@link #tag()} carries the {@code target_id}.
         * Spelled {@code VTARGET_PACK_EMPTY} (no underscore) before 0.7.0.
         */
        V_TARGET_PACK_EMPTY
    }

    private final Kind kind;
    private final Long tag; // nullable — a KLV tag, or a VTarget target_id, depending on kind

    /** Construct with no associated tag (e.g. {@code BUFFER_TOO_SMALL}). */
    public KlvEncodeException(Kind kind, String message) {
        this(kind, null, message);
    }

    /** Construct with an associated KLV tag (e.g. {@code OUT_OF_RANGE}). */
    public KlvEncodeException(Kind kind, Long tag, String message) {
        super(message);
        this.kind = kind;
        this.tag = tag;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }

    /**
     * @return the numeric identifier the error carries, or empty for variants
     *     that carry none. For most variants this is the offending KLV tag; for
     *     {@code V_TARGET_PACK_EMPTY} / {@code DUPLICATE_TARGET_ID} it is the
     *     VTarget Pack {@code target_id}.
     */
    public Optional<Long> tag() {
        return Optional.ofNullable(tag);
    }
}
