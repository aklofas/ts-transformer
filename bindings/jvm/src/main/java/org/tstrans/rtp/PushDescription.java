package org.tstrans.rtp;

import java.util.List;
import java.util.Objects;
import org.tstrans.NativeLoader;

/**
 * What an RTSP publish session announces to the server: one MPEG-TS track, or one
 * H.264 or H.265 video track with, optionally, a KLV track beside it (RFC 6597).
 * {@link RtspClient#push} sends it as the ANNOUNCE's SDP.
 *
 * <p>Each parameter set is one NAL unit with its header and without a start code. An
 * H.264 description needs at least one SPS and one PPS among its parameter sets (a NAL
 * unit of another type is ignored); an H.265 description needs a VPS, an SPS and a PPS.
 * A description that lacks one is refused by {@link RtspClient#push} with
 * {@link org.tstrans.RtspException} of kind {@code PROTOCOL}, before the ANNOUNCE is
 * written. The {@code *FromKeyFrame} factories read the parameter sets out of a key
 * frame as an encoder emits it.
 *
 * <p>Immutable: every factory copies its arrays.
 */
public final class PushDescription {
    static { NativeLoader.load(); }

    // The description's kind as the native push reads it: explicit codes, not an
    // enum ordinal.
    static final int KIND_MP2T = 0;
    static final int KIND_H264 = 1;
    static final int KIND_H265 = 2;

    private final int kind;
    private final boolean klv;
    // H.264: the parameter sets in the caller's order; H.265: VPS, SPS, PPS; MP2T: none.
    private final byte[][] parameterSets;

    private PushDescription(int kind, boolean klv, byte[][] parameterSets) {
        this.kind = kind;
        this.klv = klv;
        this.parameterSets = parameterSets;
    }

    /**
     * One MPEG-TS track (RFC 2250, payload type 33). Send whole TS packets with
     * {@link RtspPushSession#sendTs}, or convert the session with
     * {@link RtspPushSession#intoMuxSender} or {@link RtspPushSession#intoSender}.
     *
     * @return the MPEG-TS description
     */
    public static PushDescription mp2t() {
        return new PushDescription(KIND_MP2T, false, new byte[0][]);
    }

    /**
     * One H.264 track (RFC 6184, packetization mode 1) and, with {@code klv}, one KLV
     * track. Send with {@link RtspPushSession#sendVideo} and
     * {@link RtspPushSession#sendKlv}.
     *
     * @param parameterSets the stream's SPS and PPS NAL units, each without a start code
     * @param klv announce a KLV track beside the video
     * @return the H.264 description
     * @throws NullPointerException if {@code parameterSets} or one of its elements is
     *     {@code null}
     */
    public static PushDescription h264(List<byte[]> parameterSets, boolean klv) {
        Objects.requireNonNull(parameterSets, "parameterSets");
        byte[][] sets = new byte[parameterSets.size()][];
        for (int i = 0; i < sets.length; i++) {
            sets[i] = Objects.requireNonNull(parameterSets.get(i), "parameterSets element").clone();
        }
        return new PushDescription(KIND_H264, klv, sets);
    }

    /**
     * An H.264 description whose parameter sets are every SPS and PPS of
     * {@code annexB}, one access unit in Annex B form (a key frame as an encoder emits
     * it).
     *
     * @param annexB the access unit, NAL units behind start codes
     * @param klv announce a KLV track beside the video
     * @return the H.264 description
     * @throws IllegalArgumentException if the access unit carries no SPS or no PPS
     * @throws NullPointerException if {@code annexB} is {@code null}
     */
    public static PushDescription h264FromKeyFrame(byte[] annexB, boolean klv) {
        Objects.requireNonNull(annexB, "annexB");
        return new PushDescription(KIND_H264, klv, nH264ParameterSets(annexB));
    }

    /**
     * One H.265 track (RFC 7798) and, with {@code klv}, one KLV track. Send with
     * {@link RtspPushSession#sendVideo} and {@link RtspPushSession#sendKlv}.
     *
     * @param vps the video parameter set NAL unit, without a start code
     * @param sps the sequence parameter set NAL unit
     * @param pps the picture parameter set NAL unit
     * @param klv announce a KLV track beside the video
     * @return the H.265 description
     * @throws NullPointerException if a parameter set is {@code null}
     */
    public static PushDescription h265(byte[] vps, byte[] sps, byte[] pps, boolean klv) {
        Objects.requireNonNull(vps, "vps");
        Objects.requireNonNull(sps, "sps");
        Objects.requireNonNull(pps, "pps");
        return new PushDescription(KIND_H265, klv, new byte[][] {vps.clone(), sps.clone(), pps.clone()});
    }

    /**
     * An H.265 description with the first VPS, SPS and PPS of {@code annexB}, one access
     * unit in Annex B form.
     *
     * @param annexB the access unit, NAL units behind start codes
     * @param klv announce a KLV track beside the video
     * @return the H.265 description
     * @throws IllegalArgumentException if the access unit lacks a VPS, an SPS or a PPS
     * @throws NullPointerException if {@code annexB} is {@code null}
     */
    public static PushDescription h265FromKeyFrame(byte[] annexB, boolean klv) {
        Objects.requireNonNull(annexB, "annexB");
        return new PushDescription(KIND_H265, klv, nH265ParameterSets(annexB));
    }

    /** The kind code the native push reads ({@code KIND_*}). */
    int kind() { return kind; }

    /** Whether a KLV track is announced. */
    boolean klv() { return klv; }

    /** The parameter sets, as the native push reads them (not copied). */
    byte[][] parameterSets() { return parameterSets; }

    @Override public String toString() {
        String name = switch (kind) {
            case KIND_H264 -> "H264";
            case KIND_H265 -> "H265";
            default -> "MP2T";
        };
        return "PushDescription(" + name + (klv ? ", klv" : "") + ")";
    }

    // Each returns the parameter sets as fresh arrays, or throws
    // IllegalArgumentException when the access unit lacks one.
    private static native byte[][] nH264ParameterSets(byte[] annexB);
    private static native byte[][] nH265ParameterSets(byte[] annexB);
}
