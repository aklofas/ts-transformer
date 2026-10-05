package org.tstrans.rtp;

import org.tstrans.mpegts.MuxerStats;

/**
 * Combined {@code (SocketStats, MuxerStats)} snapshot returned by
 * {@link MuxSender#stats()} and {@link DemuxReceiver#stats()}. The Java stand-in
 * for tst-py's {@code (SocketStats, MuxerStats)} tuple.
 *
 * <p>Distinct from {@link org.tstrans.srt.TransportStats}: this one carries an
 * {@link org.tstrans.rtp.SocketStats} (RTP wire counters), not the SRT one.
 *
 * @param socketStats the RTP transport's wire-level counters
 * @param muxerStats  the inner muxer's / demuxer's program and packet totals
 */
public record TransportStats(SocketStats socketStats, MuxerStats muxerStats) {}
