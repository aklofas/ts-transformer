package org.tstrans.shells;

import java.util.Optional;
import java.util.concurrent.atomic.AtomicReference;
import org.tstrans.rtp.RtpStats;

/**
 * The RTP counters a receiving shell read last, so that a read never goes back and a
 * read after {@code close()} still answers. Empty for a shell that is not over RTP,
 * which the value read at construction decides.
 */
final class RtpCounters {
    private final AtomicReference<RtpStats> seen;

    RtpCounters(RtpStats atOpen) { seen = new AtomicReference<>(atOpen); }

    /** Fold in {@code now} (null when the native could not read it) and answer. */
    Optional<RtpStats> read(RtpStats now) {
        if (seen.get() == null) return Optional.empty();
        if (now == null) return Optional.of(seen.get());
        return Optional.of(seen.accumulateAndGet(now, RtpCounters::later));
    }

    /** Fold in the counters the close read after its take ({@code out[0] < 0}: none). */
    void atClose(long[] out) {
        if (out[0] >= 0 && seen.get() != null) {
            seen.accumulateAndGet(new RtpStats(out[0], out[1]), RtpCounters::later);
        }
    }

    // Each counter only grows, but two reads on two threads may land in either order.
    private static RtpStats later(RtpStats a, RtpStats b) {
        return new RtpStats(Math.max(a.malformedPackets(), b.malformedPackets()),
            Math.max(a.sourceRejected(), b.sourceRejected()));
    }
}
