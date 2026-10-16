package org.tstrans.shells;

import org.tstrans.mpegts.MuxerStats;

/**
 * Combined stats snapshot returned by {@code MuxSender.stats()} and
 * {@code DemuxReceiver.stats()}. Pairs the transport's wire-level
 * {@link SocketStats} with the inner {@link MuxerStats}.
 *
 * <p>This record is the Java stand-in for tst-py's {@code (SocketStats,
 * MuxerStats)} tuple.
 *
 * @param socketStats the transport's wire-level counters
 * @param muxerStats  the inner muxer's program / packet totals
 */
public record TransportStats(SocketStats socketStats, MuxerStats muxerStats) {}
