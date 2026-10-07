package org.tstrans.rtp;

/**
 * RTP protocol-level statistics snapshot. Returned by
 * {@link H264Receiver#rtpStats()}. Mirrors tst-py {@code tstrans.rtp.RtpStats}
 * and {@code tst_rtp::transport::RtpStats}.
 *
 * <p>Counter values are unsigned; Java has no unsigned integer types, so each
 * value is the reinterpreted bit pattern as a signed {@code long}.
 */
public record RtpStats(
        /**
         * Number of received datagrams with an invalid RTP header, wrong payload
         * type, or empty payload. Cumulative since {@link H264Receiver#listen(String)}.
         */
        long malformedPackets,
        /**
         * Number of datagrams dropped because their peer IP did not match the
         * URL's {@code ?source=} pin; 0 when no pin is set.
         */
        long sourceRejected
) {}
