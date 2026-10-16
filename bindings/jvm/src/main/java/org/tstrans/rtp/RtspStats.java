package org.tstrans.rtp;

/**
 * RTSP session RTCP-derived stats snapshot. Mirrors tst-py
 * {@code tstrans.rtp.RtspStats}. Reserved — all fields always report zero until
 * RTCP session stats land; see {@code docs/project/deferred-features.md}.
 *
 * <p>Unsigned counters are reinterpreted as signed {@code long}/{@code int}; for
 * the magnitudes seen here they are effectively non-negative.
 *
 * @param rrPacketsReceived   RTCP Receiver Reports received
 * @param srPacketsReceived   RTCP Sender Reports received
 * @param interarrivalJitterUs interarrival jitter (microseconds, u32)
 * @param fractionLostQ8       fraction lost, 8-bit fixed point (u8)
 */
public record RtspStats(
        long rrPacketsReceived,
        long srPacketsReceived,
        long interarrivalJitterUs,
        int fractionLostQ8) {
}
