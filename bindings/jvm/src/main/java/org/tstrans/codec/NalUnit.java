package org.tstrans.codec;

import java.nio.ByteBuffer;

/**
 * One H.264 / H.265 / H.266 NAL unit. Tagged with {@code kind} so callers can
 * dispatch on the codec discriminant. Mirrors {@code tstrans.codec.NalUnit}.
 *
 * <p>Construct via the codec-specific static factories
 * ({@link #h264}, {@link #h265}, {@link #h266}). The codec-specific header
 * fields not applicable to a given codec are {@code null}:
 * {@code refIdc} is non-null only for H.264; {@code layerId} /
 * {@code temporalIdPlus1} are non-null only for H.265 / H.266.
 *
 * <p>{@code payload} carries the RBSP body (Annex-B start codes stripped;
 * emulation-prevention bytes preserved). The factories wrap a copy of the input
 * via {@link ByteBuffer#wrap(byte[])} so the buffer is JVM-owned (heap).
 *
 * @param kind            codec discriminant — {@code "H264"} / {@code "H265"} / {@code "H266"}
 * @param nalType         NAL unit type integer (codec-dependent width)
 * @param refIdc          H.264 {@code nal_ref_idc}, or {@code null}
 * @param layerId         H.265/H.266 {@code nuh_layer_id}, or {@code null}
 * @param temporalIdPlus1 H.265/H.266 {@code nuh_temporal_id_plus1}, or {@code null}
 * @param payload         RBSP payload bytes (heap {@code ByteBuffer})
 */
public record NalUnit(
        String kind,
        int nalType,
        Integer refIdc,
        Integer layerId,
        Integer temporalIdPlus1,
        ByteBuffer payload) implements VideoUnit {

    /**
     * Construct an H.264 NAL unit.
     *
     * @param nalType 5-bit {@code nal_unit_type} (H.264 §7.3.1)
     * @param refIdc  2-bit {@code nal_ref_idc} (H.264 §7.3.1)
     * @param payload RBSP body bytes
     * @return the H.264 NAL unit
     */
    public static NalUnit h264(int nalType, int refIdc, byte[] payload) {
        return new NalUnit("H264", nalType, refIdc, null, null, ByteBuffer.wrap(payload.clone()));
    }

    /**
     * Construct an H.265 NAL unit.
     *
     * @param nalType         6-bit {@code nal_unit_type} (H.265 §7.3.1.2)
     * @param layerId         6-bit {@code nuh_layer_id}
     * @param temporalIdPlus1 3-bit {@code nuh_temporal_id_plus1}
     * @param payload         RBSP body bytes
     * @return the H.265 NAL unit
     */
    public static NalUnit h265(int nalType, int layerId, int temporalIdPlus1, byte[] payload) {
        return new NalUnit("H265", nalType, null, layerId, temporalIdPlus1, ByteBuffer.wrap(payload.clone()));
    }

    /**
     * Construct an H.266 / VVC NAL unit.
     *
     * @param nalType         5-bit {@code nal_unit_type} (H.266 V4 §7.3.1.2)
     * @param layerId         6-bit {@code nuh_layer_id}
     * @param temporalIdPlus1 3-bit {@code nuh_temporal_id_plus1}
     * @param payload         RBSP body bytes
     * @return the H.266 NAL unit
     */
    public static NalUnit h266(int nalType, int layerId, int temporalIdPlus1, byte[] payload) {
        return new NalUnit("H266", nalType, null, layerId, temporalIdPlus1, ByteBuffer.wrap(payload.clone()));
    }
}
