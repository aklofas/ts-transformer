package org.tstrans.rtp;

/**
 * Wire shape a publisher's ANNOUNCE declared on a {@link PublishMount}. Mirrors
 * {@code tst_rtp::rtsp::server::publish::PublishShape} and tst-py's
 * {@code tstrans.rtp.PublishShape}; the declaration order matches the C ABI's
 * {@code tst_rtsp_publish_shape} values ({@code MP2T = 0}, {@code ELEMENTARY = 1}).
 */
public enum PublishShape {
    /** One MPEG-TS-over-RTP track (RFC 2250); the bytes pass through. */
    MP2T,
    /**
     * One H.264 track (RFC 6184), optionally with one KLV track (RFC 6597),
     * re-muxed into MPEG-TS (video PID 0x100, KLV PID 0x101). {@link
     * PublisherInfo#klv()} says whether the KLV track is present.
     */
    ELEMENTARY
}
