package org.tstrans.codec;

/**
 * Which MISP time base a {@link MispTimestamp} carries.
 * Mirrors {@code tst_core::codec::misp_time::MispTimeKind}.
 *
 * <p>Ordinals cross JNI as {@code jint}: {@code MICRO=0}, {@code NANO=1}.
 */
public enum MispTimeKind {
    /**
     * Microseconds since the MISP epoch (ST 0603 Precision Time Stamp).
     * Valid for H.264 and H.265 per ST 0604.6 §7/§12.1.
     */
    MICRO,
    /**
     * Nanoseconds since the MISP epoch (ST 0603 Nano Precision Time Stamp).
     * H.265-only per ST 0604.6 §12.2; using this kind on an H.264 stream
     * throws {@link org.tstrans.MuxException} with kind
     * {@code MISP_TIME}.
     */
    NANO
}
