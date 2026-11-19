package org.tstrans.rtp;

/**
 * Counters of an {@link RtspPushSession}, over all its tracks: a snapshot.
 *
 * <p>Unsigned 64-bit values are carried as the bit pattern of a signed {@code long}: a
 * value of 2^63 or more reads as negative.
 *
 * @param packetsSent RTP packets handed to the client's thread
 * @param bytesSent bytes of those packets, RTP header included
 * @param senderReportsSent RTCP sender reports handed to the client's thread
 * @param receiverReports RTCP receiver reports the server sent; a server that sends
 *     none leaves this and the two loss figures at 0
 * @param fractionLostQ8 the fraction of packets lost since the report before, in
 *     1/256, as the server reported it for the track whose receiver report came last
 * @param cumulativeLost the packets lost since the start, from the same report
 * @param framesDroppedInbound frames the server sent on a channel this side sends
 *     media on, read and dropped
 * @param framesDroppedAtClose frames still queued, not yet handed to the connection,
 *     when the session ended: they were never written
 */
public record RtspPushStats(
        long packetsSent,
        long bytesSent,
        long senderReportsSent,
        long receiverReports,
        int fractionLostQ8,
        long cumulativeLost,
        long framesDroppedInbound,
        long framesDroppedAtClose) {
}
