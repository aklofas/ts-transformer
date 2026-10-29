package org.tstrans.mpegts;

/**
 * Opaque handle for a KLV metadata stream within a configured muxer.
 *
 * <p>Obtain one from {@code MuxSender.klvHandle()} and pass it to
 * {@code MuxSender.sendKlvTo(...)} to target a specific stream. The {@code raw}
 * value is the muxer's packed {@code u32} stream identifier widened to
 * {@code long}; it addresses a stream by index.
 *
 * <p>A handle is meaningful only to the muxer that minted it; keeping it with
 * that muxer is the caller's obligation. A handle from another muxer is not
 * detected and addresses this muxer's stream at the same index. A raw value
 * with an invalid bit layout, or an index past this muxer's streams, is
 * rejected at push time with {@code INVALID_USAGE}.
 *
 * <p>Mirrors {@code tstrans.mpegts.KlvStreamHandle}. Being a record, equality
 * and hashing are by {@code raw}.
 *
 * @param raw the packed {@code u32} stream identifier, widened to {@code long}
 */
public record KlvStreamHandle(long raw) {
    /**
     * Reconstruct a handle from a raw value previously returned by
     * {@code MuxSender.klvHandle().raw()}. No validation occurs here —
     * a malformed or out-of-range handle is rejected by the native push path.
     *
     * @param raw the packed {@code u32} value, widened to {@code long}
     * @return a {@code KlvStreamHandle} wrapping {@code raw}
     */
    public static KlvStreamHandle fromRaw(long raw) {
        return new KlvStreamHandle(raw);
    }
}
