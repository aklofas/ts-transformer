package org.tstrans.srt;

/**
 * Frozen SRT-specific statistics snapshot. Mirror of {@code tst_srt::Stats} /
 * tst-py {@code tstrans.srt.SrtStats}. All counters are widened to {@code long}
 * (Java has no unsigned) except {@link #mbpsEstimatedBandwidth} which is a
 * {@code double}. For the abstract transport view shared with RTP use
 * {@link org.tstrans.shells.SocketStats}.
 */
public record SrtStats(
        long bytesSent,
        long bytesReceived,
        long bytesLostRecvSide,
        long bytesLostSendSide,
        long packetsSent,
        long packetsReceived,
        long packetsLostRecvSide,
        long packetsLostSendSide,
        long packetsRetransmitted,
        long packetsDroppedRecvSide,
        long packetsDroppedSendSide,
        long rttUs,
        long sendBandwidthBps,
        long recvBandwidthBps,
        double mbpsEstimatedBandwidth,
        long sendBufferPackets,
        long recvBufferPackets) {}
