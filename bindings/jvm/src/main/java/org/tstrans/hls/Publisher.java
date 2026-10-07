package org.tstrans.hls;

import org.tstrans.HlsException;

/**
 * Outbound-only, segment-aware MPEG-TS byte sink. Mirrors the Rust
 * {@code tst_core::publisher::Publisher} trait method for method (the rail
 * {@code scripts/check/jvm/publisher-interface-mirror.sh} keeps them equal).
 * {@link HlsPublisher} is the one implementation; {@link AutoCloseable#close()}
 * is the quiet counterpart of {@link #finish()}.
 */
public interface Publisher extends AutoCloseable {
    /** Push whole 188-byte TS packets. */
    void pushTs(byte[] tsBytes) throws HlsException;

    /** Hint that the next push starts a new segment. */
    void cutSegment() throws HlsException;

    /** Cut, recording {@code mediaDurationUs} as the closed segment's {@code #EXTINF}. */
    void cutSegmentWithDuration(long mediaDurationUs) throws HlsException;

    /** Flush, write the terminal playlist, release the sink. Consumes the publisher. */
    void finish() throws HlsException;

    /** Universal stats snapshot. */
    PublisherStats stats();
}
