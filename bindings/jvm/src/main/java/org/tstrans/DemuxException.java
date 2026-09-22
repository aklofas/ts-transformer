package org.tstrans;

/**
 * Thrown when the MPEG-TS demuxer rejects input. {@link Kind} mirrors tst-py's
 * {@code DemuxErrorKind}; every constant except {@code UNEXPECTED_EOF} maps to a
 * producing {@code tst_core::mpegts::demux::DemuxError} variant. {@code UNEXPECTED_EOF}
 * is parity-only (no Rust producer) — see its constant doc.
 */
public final class DemuxException extends BindingException {
    private static final long serialVersionUID = 1L;

    /**
     * Discriminant. Every constant except {@code UNEXPECTED_EOF} maps 1:1 to a
     * {@code tst_core::DemuxError} variant; {@code UNEXPECTED_EOF} is parity-only
     * (documented on the constant).
     */
    public enum Kind {
        SYNC_LOSS, BAD_PMT, BAD_PES,
        /**
         * Parity-only constant mirroring tst-py's vestigial {@code DemuxErrorKind.UNEXPECTED_EOF}:
         * there is NO producer in {@code tst_core::DemuxError} (the file path treats truncation as
         * clean EOF and surfaces read failures as native {@code IOException}). Exempted in
         * {@code scripts/check/jvm/error-mapping-coverage.sh}.
         */
        UNEXPECTED_EOF,
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
