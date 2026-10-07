package org.tstrans.hls;

import org.tstrans.HlsException;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.mpegts.MuxerConfig;

/**
 * Owns a muxer + an {@link HlsPublisher}: push elementary streams, it muxes to
 * MPEG-TS and feeds the HLS sink. Mirrors {@code tstrans.hls.MuxPublisher};
 * wraps {@code tst_pipeline::MuxPublisher<HlsPublisher>}.
 *
 * <p>{@link #withConfigHls} CONSUMES the publisher handle only once the native
 * shell has actually been built (it throws {@link IllegalStateException}
 * afterwards). A rejected program config leaves {@code publisher} fully
 * usable — the handle is read, never claimed, until the native call succeeds.
 * {@link #finishIntoPublisher()} consumes this shell and hands back a fresh
 * {@link HlsPublisher} to {@code finish()} or {@code finishServing()}.
 * {@link #close()} finishes both quietly. A {@code sendVideo} with
 * {@code keyFrame=true} cuts a segment first.
 *
 * <p>Errors: a muxer rejection is a {@link MuxException}; everything else is an
 * {@link HlsException} (the inner publisher's kind, {@code CLOSED} for a
 * consumed shell, {@code INTERNAL} for a poisoned lock).
 */
public final class MuxPublisher extends NativeHandle {
    static { NativeLoader.load(); }

    private MuxPublisher(long h) { setHandle(h); }

    /**
     * Build from a single-program config and an {@link HlsPublisher}. The
     * publisher is read live (not consumed) going in: a {@link MuxException}
     * from config validation leaves it fully usable, since the native side
     * rejects the config before taking the publisher out of its registry.
     * Should a failure ever occur after the take, the publisher is gone and
     * later calls on it throw {@code IllegalStateException}; today no such
     * failure path exists. Only a successful build claims the Java-side
     * handle.
     *
     * @throws IllegalStateException if {@code publisher} was already consumed/finished
     * @throws MuxException if the muxer rejects the program config
     * @throws HlsException on a publisher-side failure
     */
    public static MuxPublisher withConfigHls(HlsPublisher publisher, MuxerConfig programConfig)
            throws MuxException, HlsException {
        long ph = publisher.liveHandleForShell();
        long h = nWithConfigHls(ph,
            programConfig.programNumber(), programConfig.pmtPid(), programConfig.pcrPid(),
            programConfig.pcrIntervalMs(), programConfig.psiIntervalMs(),
            programConfig.bufferPackets(), programConfig.av1Carriage().ordinal(),
            programConfig.streamPids(), programConfig.streamKinds(),
            programConfig.streamCodecs(), programConfig.streamTypeCodes(),
            programConfig.streamCarriesPts(),
            programConfig.dataDescBytes(), programConfig.dataDescLens());
        if (h == 0) throw new HlsException(HlsException.Kind.INTERNAL, "nWithConfigHls returned 0 without throwing");
        // Only now — after the native's own registry take has already
        // succeeded — claim the Java-side handle too.
        publisher.consumeHandleForShell();
        return new MuxPublisher(h);
    }

    /** Push one Annex-B access unit; a key frame cuts a segment first. */
    public void sendVideo(byte[] nal, long pts, boolean keyFrame) throws MuxException, HlsException {
        ensureOpen("MuxPublisher is closed");
        nSendVideo(peekHandle(), nal, pts, keyFrame);
    }

    /** Push raw KLV LS bytes (the muxer adds the AU-cell header on sync streams). */
    public void sendKlv(byte[] klv, long pts, int streamIndex) throws MuxException, HlsException {
        ensureOpen("MuxPublisher is closed");
        nSendKlv(peekHandle(), klv, pts, streamIndex);
    }

    /** Push pre-framed audio (ADTS for AAC, MPEG-2 audio frames for MP2). */
    public void sendAudio(byte[] frames, long pts) throws MuxException, HlsException {
        ensureOpen("MuxPublisher is closed");
        nSendAudio(peekHandle(), frames, pts);
    }

    /** Push one subtitle payload. */
    public void sendSubtitle(byte[] payload, long pts) throws MuxException, HlsException {
        ensureOpen("MuxPublisher is closed");
        nSendSubtitle(peekHandle(), payload, pts);
    }

    /** Explicit segment-cut hint. */
    public void cutSegment() throws MuxException, HlsException {
        ensureOpen("MuxPublisher is closed");
        nCutSegment(peekHandle());
    }

    /** Consume the shell; returns the owned publisher to finish. */
    public HlsPublisher finishIntoPublisher() throws HlsException {
        long h = consumeHandle();
        if (h == 0) throw new IllegalStateException("MuxPublisher is closed");
        long ph = nFinishIntoPublisher(h);
        if (ph == 0) throw new HlsException(HlsException.Kind.INTERNAL, "nFinishIntoPublisher returned 0 without throwing");
        return new HlsPublisher(ph);
    }

    /** Shell-level stats. */
    public MuxPublisherStats stats() {
        ensureOpen("MuxPublisher is closed");
        return nStats(peekHandle());
    }

    /** Publisher-side universal stats. */
    public PublisherStats publisherStats() {
        ensureOpen("MuxPublisher is closed");
        return nPublisherStats(peekHandle());
    }

    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    @Override public String toString() {
        return peekHandle() == 0 ? "MuxPublisher(finished)" : "MuxPublisher(open)";
    }

    private static native long nWithConfigHls(long publisherHandle, int programNumber, int pmtPid,
        int pcrPid, int pcrIntervalMs, int psiIntervalMs, int bufferPackets, int av1Carriage,
        int[] streamPids, int[] streamKinds, int[] streamCodecs, int[] streamTypeCodes,
        boolean[] streamCarriesPts, byte[] dataDescBytes, int[] dataDescLens)
        throws MuxException, HlsException;
    private static native void nSendVideo(long handle, byte[] nal, long pts, boolean keyFrame)
        throws MuxException, HlsException;
    private static native void nSendKlv(long handle, byte[] klv, long pts, int streamIndex)
        throws MuxException, HlsException;
    private static native void nSendAudio(long handle, byte[] frames, long pts)
        throws MuxException, HlsException;
    private static native void nSendSubtitle(long handle, byte[] payload, long pts)
        throws MuxException, HlsException;
    private static native void nCutSegment(long handle) throws MuxException, HlsException;
    // Only HlsException is declared: MuxPublisher::finish() in tst-pipeline is
    // infallible today (it cannot drain a muxer, so no Mux(...) path exists
    // here), so mux_publisher_error can only route an HLS-side failure.
    private static native long nFinishIntoPublisher(long handle) throws HlsException;
    private static native MuxPublisherStats nStats(long handle);
    private static native PublisherStats nPublisherStats(long handle);
    private static native void nClose(long handle);
}
