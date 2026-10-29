package org.tstrans.mpegts;

/**
 * Opaque handle for a private/data elementary stream within a configured muxer.
 *
 * <p>Obtain one from {@link Muxer#dataHandles()} /
 * {@link Muxer#dataStreamHandle(int)} (or a sender's {@code dataHandle()}) and
 * pass it to the handle-targeted push family
 * ({@link Muxer#pushDataTo(DataStreamHandle, byte[], long)} and the senders'
 * {@code sendDataTo}). The
 * {@code raw} value is the muxer's packed {@code u32} stream identifier widened
 * to {@code long}; it addresses a stream by index.
 *
 * <p>A handle is meaningful only to the muxer that minted it; keeping it with
 * that muxer is the caller's obligation. A handle from another muxer is not
 * detected and addresses this muxer's stream at the same index. A raw value
 * with an invalid bit layout, or an index past this muxer's streams, is
 * rejected at push time with {@code INVALID_USAGE}.
 *
 * <p>Mirrors {@code tstrans.mpegts.DataStreamHandle}. Being a record, equality
 * and hashing are by {@code raw}.
 *
 * @param raw the packed {@code u32} stream identifier, widened to {@code long}
 */
public record DataStreamHandle(long raw) {
    /**
     * Reconstruct a handle from a raw value previously returned by
     * {@link #raw()} (e.g. across a config channel). No validation occurs here
     * (matching the other stream-handle records; unlike tst-py's
     * {@code DataStreamHandle.from_raw}, which validates) — a malformed
     * (bad bit-layout) or out-of-range handle surfaces as
     * {@code MuxException(INVALID_USAGE)} at push time.
     *
     * @param raw the packed {@code u32} value, widened to {@code long}
     * @return a {@code DataStreamHandle} wrapping {@code raw}
     */
    public static DataStreamHandle fromRaw(long raw) {
        return new DataStreamHandle(raw);
    }
}
