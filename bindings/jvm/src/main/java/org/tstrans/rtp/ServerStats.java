package org.tstrans.rtp;

/**
 * Aggregate {@link RtspServer} stats snapshot. Mirrors tst-py
 * {@code tstrans.rtp.ServerStats}.
 *
 * @param activeSessions          live accepted-and-not-closed client sessions
 * @param totalRtpPacketsSent     cumulative RTP packets across all peers + mounts
 * @param totalRtpBytesSent       cumulative RTP bytes across all peers + mounts
 * @param mounts                  number of registered mounts, of every kind
 * @param activePublishers        publish mounts that have a publisher now
 * @param totalRtpPacketsReceived RTP packets received from publishers across every
 *                                publish mount, cumulative over the server's life
 * @param totalRtpBytesReceived   bytes of those RTP packets, headers included. The
 *                                two received totals are relaxed counters beside
 *                                each mount's own, so a snapshot taken while
 *                                packets arrive may differ from the sum of the
 *                                mounts' {@link PublishMountStats} by the packets
 *                                in flight.
 */
public record ServerStats(
    long activeSessions,
    long totalRtpPacketsSent,
    long totalRtpBytesSent,
    long mounts,
    long activePublishers,
    long totalRtpPacketsReceived,
    long totalRtpBytesReceived) {}
