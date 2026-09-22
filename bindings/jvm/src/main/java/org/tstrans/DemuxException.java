package org.tstrans;

/**
 * Thrown when the MPEG-TS demuxer rejects input. Every {@link Kind} constant
 * except {@code INTERNAL} maps 1:1 to a producing
 * {@code tst_core::mpegts::demux::DemuxError} variant; {@code INTERNAL} is the
 * JNI-side event-conversion failure.
 */
public final class DemuxException extends BindingException {
    private static final long serialVersionUID = 1L;

    /**
     * Discriminant; the names are the {@code BindingErrorKind} members,
     * verified against this enum at load time by {@link NativeLoader}.
     */
    public enum Kind {
        STRICT_REJECTION, INTERNAL,
        /**
         * {@code DemuxError::Unrecoverable} — one sync-search window held no
         * packet boundary; the scanned bytes are discarded and the next feed
         * starts a fresh search.
         */
        UNRECOVERABLE,
        /**
         * {@code DemuxError::MalformedPsi} — a PSI section claimed a length
         * that cannot fit a valid PAT/PMT (structurally impossible, not a
         * checksum mismatch).
         */
        MALFORMED_PSI,
        /**
         * {@code DemuxError::MalformedPes} — a PES header declared a length
         * too short to contain its own claimed flags, so the reassembler
         * cannot make forward progress.
         */
        MALFORMED_PES,
        /**
         * {@code DemuxError::SyncBufExhausted} — the demuxer's pre-sync buffer
         * exceeded {@code DemuxerConfig.syncBufCap}; feed in smaller chunks or
         * raise the ceiling.
         */
        SYNC_BUF_EXHAUSTED
    }

    private final Kind kind;

    public DemuxException(Kind kind, String message) {
        super(message);
        this.kind = kind;
    }

    /** @return the error discriminant. */
    public Kind kind() {
        return kind;
    }
}
