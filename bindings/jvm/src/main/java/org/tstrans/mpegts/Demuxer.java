package org.tstrans.mpegts;

import java.util.Iterator;
import java.util.NoSuchElementException;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;

/**
 * Streaming MPEG-TS demuxer. Feed TS bytes, pull {@link DemuxEvent}s.
 * Mirrors {@code tstrans.mpegts.Demuxer}. One demuxer is single-threaded;
 * the consumer owns concurrency (spec §5.5).
 *
 * <pre>{@code
 * try (Demuxer d = new Demuxer()) {
 *     d.feed(tsBytes);
 *     for (DemuxEvent e : d) { ... }
 * }
 * }</pre>
 *
 * <p><strong>Event coverage.</strong> The demuxer surfaces the full
 * {@link DemuxEvent} sealed set: {@link DemuxEvent.ProgramMap}, the sample
 * records ({@link DemuxEvent.Video}, {@link DemuxEvent.Audio},
 * {@link DemuxEvent.Subtitle}, {@link DemuxEvent.UnknownSample}),
 * {@link DemuxEvent.Metadata} (KLV), {@link DemuxEvent.NonConformant}
 * (recoverable stream-quality diagnostics), {@link DemuxEvent.Discontinuity},
 * and {@link DemuxEvent.ReconnectDiscontinuity}. No event type is skipped.
 */
public final class Demuxer extends NativeHandle implements Iterable<DemuxEvent> {
    static { org.tstrans.NativeLoader.load(); }

    public Demuxer() {
        setHandle(nOpen());
    }

    /**
     * Construct a demuxer with a non-default configuration.
     *
     * <p>Ordinal contract: the {@code strict}/{@code av1} ints passed to
     * {@code nOpenWithConfig} are the Java enum ORDINALS — the Rust side maps by
     * ordinal in the SAME declaration order as {@link StrictMode} / {@link Av1CarriageMode}.
     */
    public Demuxer(DemuxerConfig cfg) {
        setHandle(nOpenWithConfig(
            cfg.strictMode().ordinal(), cfg.pesCapPerPid(), cfg.pesCapTotal(),
            cfg.cfiTolerance(), cfg.av1Carriage().ordinal(),
            cfg.auCellCapPerPid(), cfg.lenientPsiReassembly(), cfg.syncBufCap(),
            cfg.unwrapTimestamps()));
    }

    /** Feed TS bytes. @throws DemuxException on non-conformant input. */
    public void feed(byte[] bytes) throws DemuxException {
        ensureOpen("Demuxer is closed");
        nFeed(peekHandle(), bytes);
    }

    /** Flush buffered partial units (call at end of stream). */
    public void flush() {
        ensureOpen("Demuxer is closed");
        nFlush(peekHandle());
    }

    /**
     * Pull the next ready event, or {@code null} if none is queued. Every
     * {@link DemuxEvent} variant is surfaced (see the class doc) — a returned
     * {@code null} means the event queue is currently empty.
     */
    public DemuxEvent nextEvent() throws DemuxException {
        ensureOpen("Demuxer is closed");
        return nNextEvent(peekHandle());
    }

    /** Iterate already-queued events (drains the current queue; does not feed). */
    @Override
    public Iterator<DemuxEvent> iterator() {
        return new Iterator<>() {
            private DemuxEvent peeked;
            @Override public boolean hasNext() {
                if (peeked != null) return true;
                try { peeked = nextEvent(); } catch (DemuxException e) { throw new RuntimeException(e); }
                return peeked != null;
            }
            @Override public DemuxEvent next() {
                if (!hasNext()) throw new NoSuchElementException();
                DemuxEvent e = peeked; peeked = null; return e;
            }
        };
    }

    @Override
    protected void nativeClose(long h) { nClose(h); }

    private static native long nOpen();
    private static native long nOpenWithConfig(int strict, long pesCapPerPid, long pesCapTotal,
            boolean cfi, int av1, long auCellCap, boolean lenientPsi, long syncBufCap,
            boolean unwrapTimestamps);
    private static native void nFeed(long handle, byte[] bytes) throws DemuxException;
    private static native void nFlush(long handle);
    private static native DemuxEvent nNextEvent(long handle) throws DemuxException;
    private static native void nClose(long handle);
}
