package org.tstrans.rtp;

/**
 * How a publish mount aligns its announced tracks to one clock, read from
 * {@link PublishMountStats#alignment()}. Only an elementary publisher with a KLV
 * track needs alignment. Mirrors
 * {@code tst_rtp::rtsp::server::publish::ClockAlignment} and tst-py's
 * {@code tstrans.rtp.ClockAlignment}; the declaration order matches the C ABI's
 * {@code tst_rtsp_clock_alignment} values (0 to 3).
 */
public enum ClockAlignment {
    /**
     * Nothing to align: an MP2T or video-only publisher, or no publisher has
     * announced yet.
     */
    NOT_APPLICABLE,
    /**
     * A KLV track was announced but its clock is not yet related to the video
     * clock: KLV units are held until RTCP sender reports arrive for both tracks
     * or the two-second fallback engages. Also reported after a source restart
     * (an SSRC change) until alignment is re-established. A mount that stays here
     * receives no KLV.
     */
    PENDING,
    /** Aligned by first-packet coincidence: sender reports did not arrive in time. */
    PROVISIONAL,
    /** Aligned through RTCP sender reports (RFC 3550 §6.4.1). */
    SENDER_REPORT
}
