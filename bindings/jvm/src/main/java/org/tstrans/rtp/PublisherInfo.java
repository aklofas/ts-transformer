package org.tstrans.rtp;

/**
 * The publisher that holds a {@link PublishMount}, from {@link
 * PublishMount#publisher()}. Mirrors tst-py's {@code tstrans.rtp.PublisherInfo}.
 *
 * @param peer        address ({@code "ip:port"}) of the publisher's RTSP control
 *                    connection
 * @param shape       wire shape the publisher's ANNOUNCE declared
 * @param klv         {@code true} when an elementary announce carries a KLV track
 *                    beside the video; always {@code false} for {@link
 *                    PublishShape#MP2T}
 * @param sinceUnixMs when the ANNOUNCE claimed the mount, in milliseconds since
 *                    the Unix epoch
 * @param generation  the mount's publisher generation while this publisher holds
 *                    it (the count of publishers that ended on the mount before it)
 */
public record PublisherInfo(
    String peer,
    PublishShape shape,
    boolean klv,
    long sinceUnixMs,
    long generation) {}
