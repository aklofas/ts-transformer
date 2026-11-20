package org.tstrans.rtp;

/**
 * Controls whether out-of-band SPS/PPS are injected before IDR frames.
 *
 * <p>Mirrors {@code tst_rtp::ParameterSetInjection}. The JNI layer
 * passes {@link #code()} to Rust; the codes are stable across releases.
 *
 * <ul>
 *   <li>{@link #NONE} — pass NALUs through exactly as received.
 *   <li>{@link #BEFORE_IDR} — inject cached SPS and PPS NALUs before every IDR
 *       frame (the default). Enables random-access decoding without re-signalling
 *       parameter sets out-of-band.
 * </ul>
 */
public enum ParameterSetInjection {
    /** No injection — NALUs are passed through exactly as received. Code 0. */
    NONE(0),
    /** Inject cached SPS + PPS before every IDR frame. Code 1. Default. */
    BEFORE_IDR(1);

    private final int code;

    ParameterSetInjection(int code) {
        this.code = code;
    }

    /** @return the value native code receives for this constant; stable across releases. */
    public int code() {
        return code;
    }
}
