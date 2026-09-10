package org.tstrans.pipeline;

import java.util.List;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.DemuxerStats;

/**
 * Byte-feeding KLV↔video pairer. Feed TS bytes, collect {@link PairerOutput}s.
 * Mirrors {@code tstrans.pipeline.Pairer}; wraps the core
 * {@code tst_pipeline::ext::pairing::PairingDemuxer}. Single-threaded — the
 * consumer owns concurrency (the {@code org.tstrans.mpegts.Demuxer} contract).
 *
 * <pre>{@code
 * try (Pairer p = new Pairer(0x101, 0x102)) {
 *     for (PairerOutput o : p.feed(tsBytes)) { ... }
 *     for (PairerOutput o : p.flush()) { ... }
 * }
 * }</pre>
 */
public final class Pairer extends NativeHandle {
    static { org.tstrans.NativeLoader.load(); }

    /** Construct for the given video + KLV PIDs with default configs. */
    public Pairer(int videoPid, int klvPid) {
        setHandle(nOpen(videoPid, klvPid));
    }

    /**
     * Construct with explicit configs.
     *
     * <p>Ordinal contract: the {@code strict}/{@code av1} ints passed to
     * {@code nOpenWithConfig} are the Java enum ORDINALS — the Rust side maps by
     * ordinal in the SAME declaration order as {@link org.tstrans.mpegts.StrictMode}
     * / {@link org.tstrans.mpegts.Av1CarriageMode}.
     */
    public Pairer(int videoPid, int klvPid, PairingDemuxerConfig config) {
        if (config == null) throw new IllegalArgumentException("config must be non-null");
        PairerConfig pc = config.pairer();
        boolean buffered = pc.mode() instanceof PairerMode.Buffered;
        long maxLagNanos = buffered ? ((PairerMode.Buffered) pc.mode()).maxLag().toNanos() : 0L;
        DemuxerConfig dx = config.demuxer();
        setHandle(nOpenWithConfig(videoPid, klvPid,
            buffered, maxLagNanos, pc.tolerance().toNanos(),
            pc.maxBufferedKlv(), pc.maxBufferedVideo(),
            dx != null,
            dx != null ? dx.strictMode().ordinal() : 0,
            dx != null ? dx.pesCapPerPid() : 0L,
            dx != null ? dx.pesCapTotal() : 0L,
            dx != null && dx.cfiTolerance(),
            dx != null ? dx.av1Carriage().ordinal() : 0,
            dx != null ? dx.auCellCapPerPid() : 0L,
            dx != null && dx.lenientPsiReassembly(),
            dx != null ? dx.syncBufCap() : 0L,
            dx != null && dx.unwrapTimestamps()));
    }

    /** Feed TS bytes; returns the pairing outputs produced. @throws DemuxException on non-conformant input. */
    @SuppressWarnings("unchecked")
    public List<PairerOutput> feed(byte[] bytes) throws DemuxException {
        ensureOpen("Pairer is closed");
        return (List<PairerOutput>) nFeed(peekHandle(), bytes);
    }

    /**
     * Drain end-of-stream state. Returns any unused KLV history as trailing
     * {@code UnpairedKlv} (in <em>both</em> modes — e.g. metadata after the last
     * video access unit), plus the buffered video access units in Buffered mode.
     * Most load-bearing in Buffered mode, but call it at end-of-stream in either
     * mode or trailing metadata can be dropped.
     */
    @SuppressWarnings("unchecked")
    public List<PairerOutput> flush() {
        ensureOpen("Pairer is closed");
        return (List<PairerOutput>) nFlush(peekHandle());
    }

    /** Pairing counters. */
    public PairerStats stats() {
        ensureOpen("Pairer is closed");
        return nStats(peekHandle());
    }

    /** Underlying demuxer counters. */
    public DemuxerStats demuxerStats() {
        ensureOpen("Pairer is closed");
        return nDemuxerStats(peekHandle());
    }

    /** Reset the pairing counters (not demuxer stats). */
    public void resetStats() {
        ensureOpen("Pairer is closed");
        nResetStats(peekHandle());
    }

    @Override
    protected void nativeClose(long h) { nClose(h); }

    private static native long nOpen(int videoPid, int klvPid);
    private static native long nOpenWithConfig(int videoPid, int klvPid,
        boolean buffered, long maxLagNanos, long toleranceNanos,
        long maxBufferedKlv, long maxBufferedVideo,
        boolean hasDemuxerConfig, int strict, long pesCapPerPid, long pesCapTotal,
        boolean cfi, int av1, long auCellCap, boolean lenientPsi, long syncBufCap,
        boolean unwrapTimestamps);
    private static native Object nFeed(long handle, byte[] bytes) throws DemuxException;
    private static native Object nFlush(long handle);
    private static native PairerStats nStats(long handle);
    private static native DemuxerStats nDemuxerStats(long handle);
    private static native void nResetStats(long handle);
    private static native void nClose(long handle);
}
