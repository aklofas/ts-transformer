package org.tstrans.rtp;

import java.net.InetSocketAddress;
import java.time.Instant;
import org.tstrans.internal.NativeAddr;

/**
 * The publisher that holds a {@link PublishMount}, from {@link
 * PublishMount#publisher()}. Mirrors tst-py's {@code tstrans.rtp.PublisherInfo}.
 *
 * @param peer        address of the publisher's RTSP control connection
 * @param shape       wire shape the publisher's ANNOUNCE declared
 * @param klv         {@code true} when an elementary announce carries a KLV track
 *                    beside the video; always {@code false} for {@link
 *                    PublishShape#MP2T}
 * @param since       when the ANNOUNCE claimed the mount (the server's wall clock)
 * @param generation  the mount's publisher generation while this publisher holds
 *                    it (the count of publishers that ended on the mount before it)
 */
public record PublisherInfo(
    InetSocketAddress peer,
    PublishShape shape,
    boolean klv,
    Instant since,
    long generation) {

    /** Built by the native {@code publisher()}: the peer as address bytes, the time as seconds + nanoseconds. */
    static PublisherInfo fromNative(byte[] peer, PublishShape shape, boolean klv,
            long sinceSeconds, long sinceNanos, long generation) {
        return new PublisherInfo(NativeAddr.decode(peer), shape, klv,
            Instant.ofEpochSecond(sinceSeconds, sinceNanos), generation);
    }
}
