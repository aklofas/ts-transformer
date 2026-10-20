package org.tstrans.webrtc;

/**
 * Server-wide counters, a snapshot of {@code tst_webrtc::ServerStats}.
 *
 * <p>Unsigned 64-bit values are carried as the bit pattern of a signed {@code long}: a
 * value of 2^63 or more reads as negative.
 *
 * @param mounts mounts currently added
 * @param activeSessions viewer sessions currently open
 * @param sessionsTotal viewer sessions ever admitted
 * @param sessionsRejectedCap offers refused because {@code max_viewers} were connected
 * @param sessionsRejectedCodec offers refused because they shared no usable codec
 * @param sessionsRejectedAuth requests refused for a missing or wrong bearer token
 * @param sessionsReapedIceTimeout sessions closed because they did not connect in time
 * @param sessionsReapedDisconnected sessions closed after staying disconnected, or with a
 *     full queue, too long
 * @param rtpPacketsSent RTP packets sent to every viewer
 * @param rtpBytesSent RTP bytes sent to every viewer
 * @param datagramsUnrouted datagrams on the media port that matched no session
 */
public record ServerStats(long mounts, long activeSessions, long sessionsTotal,
        long sessionsRejectedCap, long sessionsRejectedCodec, long sessionsRejectedAuth,
        long sessionsReapedIceTimeout, long sessionsReapedDisconnected, long rtpPacketsSent,
        long rtpBytesSent, long datagramsUnrouted) {}
