package org.tstrans;

/**
 * Thrown when a codec parameter-set / payload-unit parser rejects input.
 * {@link Kind} mirrors the variant set of {@code tst_core::codec::CodecParseError}
 * (12 variants); the variant-specific diagnostic fields are forwarded as nullable
 * boxed accessors (non-null only for the variant that carries them).
 *
 * <p>Mirrors tst-py's {@code tstrans.exceptions.CodecError} + {@code CodecErrorKind}.
 */
public final class CodecParseException extends BindingException {
    private static final long serialVersionUID = 1L;

    /** Discriminant; values map 1:1 from the Rust {@code CodecParseError} variants. */
    public enum Kind {
        TRUNCATED_RBSP, INVALID_GOLOMB, RESERVED_VALUE, UNSUPPORTED_PROFILE,
        DANGLING_SPS_REFERENCE, DANGLING_VPS_REFERENCE, ENGINE_ERROR,
        INVALID_LEB128, BAD_SYNC_WORD, TRUNCATED, FORBIDDEN, UNSUPPORTED_FREE_FORMAT,
        /**
         * {@code CodecParseError::InvalidLengthSize} — the AVCC/HVCC length
         * prefix size is not 1, 2 or 4 bytes. Folded into
         * {@code ENGINE_ERROR} before 0.7.0.
         */
        INVALID_LENGTH_SIZE,
        /**
         * {@code CodecParseError::NalLengthOverflow} — a NAL length does not
         * fit the configured length-prefix size. Folded into
         * {@code ENGINE_ERROR} before 0.7.0.
         */
        NAL_LENGTH_OVERFLOW,
        /**
         * {@code CodecParseError::BufferTooSmall} — the caller's output buffer
         * cannot hold the converted stream. Folded into {@code ENGINE_ERROR}
         * before 0.7.0.
         */
        BUFFER_TOO_SMALL
    }

    private final Kind kind;
    private final String codec;
    private final Integer offsetBits;
    private final Integer neededBits;
    private final String field;
    private final Integer value;
    private final Integer profileIdc;
    private final Integer spsId;
    private final Integer vpsId;
    private final Integer offsetBytes;
    private final Integer expected;
    private final Integer found;
    private final Integer needed;
    private final Integer had;
    private final Integer layer;

    /**
     * Canonical constructor used by the JNI marshalling path. Absent variant
     * fields are passed as {@code null}; the per-variant population mirrors
     * tst-py's {@code codec_parse_error_to_pyerr}.
     *
     * @param kind       the error discriminant
     * @param codec      short lowercase codec name (e.g. {@code "h264"})
     * @param message    human-readable diagnostic
     * @param offsetBits bit offset (TRUNCATED_RBSP / INVALID_GOLOMB)
     * @param neededBits bit shortfall (TRUNCATED_RBSP)
     * @param field      field name (RESERVED_VALUE / FORBIDDEN)
     * @param value      reserved value (RESERVED_VALUE)
     * @param profileIdc profile_idc (UNSUPPORTED_PROFILE)
     * @param spsId      SPS id (DANGLING_SPS_REFERENCE)
     * @param vpsId      VPS id (DANGLING_VPS_REFERENCE)
     * @param offsetBytes byte offset (INVALID_LEB128)
     * @param expected   expected sync word (BAD_SYNC_WORD)
     * @param found      found sync word (BAD_SYNC_WORD)
     * @param needed     bytes needed (TRUNCATED)
     * @param had        bytes had (TRUNCATED)
     * @param layer      MPEG audio layer (UNSUPPORTED_FREE_FORMAT)
     */
    public CodecParseException(
            Kind kind,
            String codec,
            String message,
            Integer offsetBits,
            Integer neededBits,
            String field,
            Integer value,
            Integer profileIdc,
            Integer spsId,
            Integer vpsId,
            Integer offsetBytes,
            Integer expected,
            Integer found,
            Integer needed,
            Integer had,
            Integer layer) {
        super(message);
        this.kind = kind;
        this.codec = codec;
        this.offsetBits = offsetBits;
        this.neededBits = neededBits;
        this.field = field;
        this.value = value;
        this.profileIdc = profileIdc;
        this.spsId = spsId;
        this.vpsId = vpsId;
        this.offsetBytes = offsetBytes;
        this.expected = expected;
        this.found = found;
        this.needed = needed;
        this.had = had;
        this.layer = layer;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }

    /** @return the short lowercase codec name that produced this error. */
    public String codec() {
        return codec;
    }

    /** @return bit offset (TRUNCATED_RBSP / INVALID_GOLOMB), or {@code null}. */
    public Integer offsetBits() {
        return offsetBits;
    }

    /** @return bit shortfall (TRUNCATED_RBSP), or {@code null}. */
    public Integer neededBits() {
        return neededBits;
    }

    /** @return field name (RESERVED_VALUE / FORBIDDEN), or {@code null}. */
    public String field() {
        return field;
    }

    /** @return reserved field value (RESERVED_VALUE), or {@code null}. */
    public Integer value() {
        return value;
    }

    /** @return profile_idc (UNSUPPORTED_PROFILE), or {@code null}. */
    public Integer profileIdc() {
        return profileIdc;
    }

    /** @return SPS id (DANGLING_SPS_REFERENCE), or {@code null}. */
    public Integer spsId() {
        return spsId;
    }

    /** @return VPS id (DANGLING_VPS_REFERENCE), or {@code null}. */
    public Integer vpsId() {
        return vpsId;
    }

    /** @return byte offset (INVALID_LEB128), or {@code null}. */
    public Integer offsetBytes() {
        return offsetBytes;
    }

    /** @return expected sync word (BAD_SYNC_WORD), or {@code null}. */
    public Integer expected() {
        return expected;
    }

    /** @return found sync word (BAD_SYNC_WORD), or {@code null}. */
    public Integer found() {
        return found;
    }

    /** @return bytes needed (TRUNCATED), or {@code null}. */
    public Integer needed() {
        return needed;
    }

    /** @return bytes had (TRUNCATED), or {@code null}. */
    public Integer had() {
        return had;
    }

    /** @return MPEG audio layer (UNSUPPORTED_FREE_FORMAT), or {@code null}. */
    public Integer layer() {
        return layer;
    }
}
