package org.tstrans.webrtc;

/**
 * One viewer session, a snapshot of {@code tst_webrtc::SessionStats}.
 *
 * <p>Unsigned 64-bit values are carried as the bit pattern of a signed {@code long}: a
 * value of 2^63 or more reads as negative.
 *
 * @param id the session id, the last segment of its WHEP resource path
 * @param mountPath the mount the session plays
 * @param remoteAddr the viewer's address as {@code "ip:port"} once ICE has selected one,
 *     else {@code null}
 * @param iceState {@code "new"}, {@code "checking"}, {@code "connected"},
 *     {@code "disconnected"}, {@code "failed"} or {@code "closed"}; {@code "unknown"}
 *     for a state the Rust library adds before this binding names it
 * @param connectedForMillis how long the session has been connected, or {@code -1}
 *     before it connected
 * @param rtpPackets RTP packets sent to the viewer
 * @param rtpBytes RTP bytes sent to the viewer
 * @param nacks NACKs received from the viewer
 * @param plis picture loss indications received from the viewer
 * @param dcMessages KLV messages sent on the viewer's data channel
 * @param packetsDroppedQueueFull RTP payloads dropped because the viewer's queue was full
 * @param dcMessagesDropped KLV messages dropped for the viewer: its queue was full, or its
 *     data channel refused or failed the write
 */
public record SessionStats(String id, String mountPath, String remoteAddr, String iceState,
        long connectedForMillis, long rtpPackets, long rtpBytes, long nacks, long plis,
        long dcMessages, long packetsDroppedQueueFull, long dcMessagesDropped) {}
