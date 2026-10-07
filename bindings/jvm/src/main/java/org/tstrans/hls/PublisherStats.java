package org.tstrans.hls;

import java.util.OptionalLong;

/**
 * Universal cross-publisher stats snapshot. Mirrors
 * {@code tst_core::publisher::PublisherStats}. The two optional durations are
 * carried as raw microseconds with {@code -1} meaning absent (JNI builds the
 * record from four longs); use the {@link OptionalLong} accessors.
 */
public record PublisherStats(long segmentsWritten, long bytesWritten,
                             long currentSegmentAgeUsRaw, long lastSegmentDurationUsRaw) {
    /** Age of the open segment, microseconds; empty when no segment is open. */
    public OptionalLong currentSegmentAgeUs() {
        return currentSegmentAgeUsRaw < 0 ? OptionalLong.empty() : OptionalLong.of(currentSegmentAgeUsRaw);
    }

    /** Duration of the last closed segment, microseconds; empty before the first cut. */
    public OptionalLong lastSegmentDurationUs() {
        return lastSegmentDurationUsRaw < 0 ? OptionalLong.empty() : OptionalLong.of(lastSegmentDurationUsRaw);
    }
}
