package org.tstrans.rtp;

/**
 * Snapshot of a {@link PublishMount}'s stats, from {@link PublishMount#stats()}.
 * Counters are cumulative over the mount's life, across publishers. Mirrors
 * {@code tst_rtp::rtsp::server::publish::PublishMountStats} field for field, in
 * the same order, and tst-py's {@code tstrans.rtp.PublishMountStats}.
 *
 * @param rtpPacketsReceived   RTP packets received from publishers, counted
 *                             before validation
 * @param bytesReceived        bytes of those RTP packets, headers included
 * @param malformedPackets     packets dropped as unusable: not RTP, the wrong
 *                             payload type, an invalid MP2T payload, or an
 *                             interleaved frame on an unknown channel
 * @param sourceRejected       UDP datagrams dropped because they came from an IP
 *                             other than the publisher's control connection
 * @param framesEmitted        frames emitted to the mount's sinks (PLAY readers
 *                             and the application transport)
 * @param framesDroppedApp     frames dropped because the application transport's
 *                             queue was full (the application stopped reading its
 *                             {@link DemuxReceiver})
 * @param framesDroppedReaders frames dropped across PLAY readers that lagged
 *                             behind the fan-out
 * @param ausEmitted           access units emitted by an elementary-shape adapter
 * @param ausDropped           access units an elementary-shape adapter dropped
 * @param ausReordered         access units muxed with a PTS below one already
 *                             muxed: nonzero means the publisher sends B-frames
 * @param klvUnitsEmitted      KLV units emitted by an elementary-shape adapter
 * @param klvUnitsDropped      KLV units an elementary-shape adapter dropped
 * @param alignment            how the current publisher's tracks are aligned to
 *                             one clock
 * @param alignmentSteps       times a new clock mapping replaced the previous one
 * @param ssrcChanges          source restarts (RTP SSRC changes) on an elementary
 *                             publisher's tracks, counted once per change per track
 * @param generation           publishers that have ended on this mount
 * @param peerCount            live PLAY readers subscribed to the mount's fan-out
 */
public record PublishMountStats(
    long rtpPacketsReceived,
    long bytesReceived,
    long malformedPackets,
    long sourceRejected,
    long framesEmitted,
    long framesDroppedApp,
    long framesDroppedReaders,
    long ausEmitted,
    long ausDropped,
    long ausReordered,
    long klvUnitsEmitted,
    long klvUnitsDropped,
    ClockAlignment alignment,
    long alignmentSteps,
    long ssrcChanges,
    long generation,
    long peerCount) {}
