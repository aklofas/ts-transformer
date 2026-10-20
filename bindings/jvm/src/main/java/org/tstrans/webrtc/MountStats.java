package org.tstrans.webrtc;

/**
 * One mount's counters, a snapshot of {@code tst_webrtc::MountStats}.
 *
 * <p>Unsigned 64-bit values are carried as the bit pattern of a signed {@code long}: a
 * value of 2^63 or more reads as negative.
 *
 * @param framesPushed access units accepted by {@link MountHandle#pushVideo}
 * @param keyframesPushed of those, the keyframes (an IRAP picture with its parameter sets)
 * @param keyframeFlagMismatch pushes whose {@code keyFrame} flag disagreed with the NAL units
 * @param viewers viewers currently watching
 * @param viewersWaitingKeyframe of those, the viewers still waiting for a keyframe
 * @param klvRecordsPushed KLV records pushed
 * @param klvRecordsDroppedUnbound KLV records dropped before any frame carried them, because
 *     the queue of records waiting for a frame was full
 * @param klvSeiInserted KLV records carried in an SEI message
 * @param klvSeiDroppedOversize KLV records left out of the SEI because they did not fit the
 *     access unit's SEI budget
 * @param klvDcMessagesSent KLV data-channel messages sent
 * @param klvDcDroppedOversize KLV records over the data-channel limit
 * @param klvDcDroppedViewer KLV data-channel messages dropped on the way to viewers (a full
 *     queue, a refused or failed write); the video is sent regardless
 * @param audioFramesPushed audio packets accepted for the viewers
 * @param audioFramesDropped audio pushes discarded
 * @param plisReceived picture loss indications received from viewers
 * @param nacksReceived NACKs received from viewers
 * @param viewerPacketsDroppedQueueFull video RTP payloads and Opus packets dropped because a
 *     queue was full
 * @param rtpClockRebases times the RTP clock was rebased after a PTS jump
 * @param demuxNonconformant non-conformances reported by an MP2T mount's demuxer, and the
 *     units it skips
 */
public record MountStats(long framesPushed, long keyframesPushed, long keyframeFlagMismatch,
        long viewers, long viewersWaitingKeyframe, long klvRecordsPushed,
        long klvRecordsDroppedUnbound, long klvSeiInserted, long klvSeiDroppedOversize,
        long klvDcMessagesSent, long klvDcDroppedOversize, long klvDcDroppedViewer,
        long audioFramesPushed, long audioFramesDropped, long plisReceived, long nacksReceived,
        long viewerPacketsDroppedQueueFull, long rtpClockRebases, long demuxNonconformant) {}
