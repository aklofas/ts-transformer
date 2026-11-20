package org.tstrans.webrtc;

import java.net.InetSocketAddress;
import java.time.Duration;
import java.util.Optional;
import org.tstrans.internal.NativeAddr;

/**
 * One viewer session, a snapshot of {@code tst_webrtc::SessionStats}.
 *
 * <p>Unsigned 64-bit values are carried as the bit pattern of a signed {@code long}: a
 * value of 2^63 or more reads as negative.
 *
 * @param id the session id, the last segment of its WHEP resource path
 * @param mountPath the mount the session plays
 * @param remoteAddr the viewer's address once ICE has selected one, else empty
 * @param iceState {@code "new"}, {@code "checking"}, {@code "connected"},
 *     {@code "disconnected"}, {@code "failed"} or {@code "closed"}; {@code "unknown"}
 *     for a state the Rust library adds before this binding names it
 * @param connectedFor how long the session has been connected, or empty before it
 *     connected
 * @param rtpPackets RTP packets sent to the viewer
 * @param rtpBytes RTP bytes sent to the viewer
 * @param nacks NACKs received from the viewer
 * @param plis picture loss indications received from the viewer
 * @param dcMessages KLV messages sent on the viewer's data channel
 * @param packetsDroppedQueueFull RTP payloads dropped because the viewer's queue was full
 * @param dcMessagesDropped KLV messages dropped for the viewer: its queue was full, or its
 *     data channel refused or failed the write
 */
public record SessionStats(String id, String mountPath, Optional<InetSocketAddress> remoteAddr,
        String iceState, Optional<Duration> connectedFor, long rtpPackets, long rtpBytes,
        long nacks, long plis, long dcMessages, long packetsDroppedQueueFull,
        long dcMessagesDropped) {

    /**
     * Built by the native {@code sessions()}: the address as bytes ({@code null} for none),
     * the connected time in nanoseconds ({@code -1} before connected).
     */
    static SessionStats fromNative(String id, String mountPath, byte[] remoteAddr,
            String iceState, long connectedForNanos, long rtpPackets, long rtpBytes, long nacks,
            long plis, long dcMessages, long packetsDroppedQueueFull, long dcMessagesDropped) {
        return new SessionStats(id, mountPath, Optional.ofNullable(NativeAddr.decode(remoteAddr)),
            iceState,
            connectedForNanos < 0 ? Optional.empty() : Optional.of(Duration.ofNanos(connectedForNanos)),
            rtpPackets, rtpBytes, nacks, plis, dcMessages, packetsDroppedQueueFull,
            dcMessagesDropped);
    }
}
