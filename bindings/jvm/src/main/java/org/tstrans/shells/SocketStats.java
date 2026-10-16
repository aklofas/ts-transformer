package org.tstrans.shells;

/**
 * Frozen wire-level statistics snapshot, the same sixteen counters on every
 * transport. Mirror of {@code tst_core::transport::SocketStats} and tst-py's
 * {@code tstrans.shells.SocketStats}. All counters are widened to {@code long}
 * (Java has no unsigned). For SRT-specific extras (estimated bandwidth,
 * symmetric loss split) use {@code srtStats()} on an {@code srt://} shell.
 */
public record SocketStats(
        long rttUs,
        long sendBandwidthBps,
        long recvBandwidthBps,
        long linkBandwidthBps,
        long bytesSent,
        long packetsSent,
        long bytesReceived,
        long packetsReceived,
        long bytesLostRecv,
        long packetsLostRecv,
        long packetsLostSend,
        long packetsRetransmitted,
        long packetsDroppedSend,
        long packetsDroppedRecv,
        long sendBufferPackets,
        long recvBufferPackets) {}
