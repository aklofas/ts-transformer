package org.tstrans.rtp;

import java.util.Optional;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * A publish mount on an {@link RtspServer}: the server accepts ANNOUNCE / SETUP
 * {@code mode=record} / RECORD on its path and hands the received MPEG-TS to the
 * application through {@link #intoDemuxReceiver()}, while PLAY readers on the same
 * path are re-served from the same bytes. One publisher holds the mount at a time.
 * Mirrors tst-py's {@code tstrans.rtp.PublishMount}.
 *
 * <p>Returned by {@link RtspServer#addPublishMount(String)} and, for a mount an
 * ANNOUNCE created on demand, by {@link RtspServer#nextPublisher(long)}.
 *
 * <p><b>Lifetime:</b> this object stays usable for {@link #stats()}, {@link
 * #publisher()} and {@link #generation()} after its transport is taken, after
 * {@link RtspServer#removeMount(String)}, and after the server stops. {@link
 * #close()} frees only this wrapper: the mount stays registered in the server
 * until {@code removeMount} or the server stops.
 *
 * <p><b>Thread safety:</b> every method returns without parking, and the
 * underlying handle is shared, so any method may be called from any thread,
 * including a concurrent {@link #close()} (a racing call either runs or throws a
 * clean {@link IllegalStateException}).
 */
public final class PublishMount extends NativeHandle {
    static { NativeLoader.load(); }

    PublishMount(long h) { setHandle(h); }

    /** The mount path ({@code "/path"}). */
    public String mountPath() { return nMountPath(requireOpen(CLOSED)); }

    /** Live PLAY readers subscribed to the mount's fan-out. */
    public long peerCount() { return nPeerCount(requireOpen(CLOSED)); }

    /** Publishers that have ended on this mount. */
    public long generation() { return nGeneration(requireOpen(CLOSED)); }

    /** The publisher that holds the mount now, or empty when none does. */
    public Optional<PublisherInfo> publisher() {
        return Optional.ofNullable(nPublisher(requireOpen(CLOSED)));
    }

    /** Snapshot of the mount's cumulative and live stats. */
    public PublishMountStats stats() { return nStats(requireOpen(CLOSED)); }

    /**
     * End the application side only: a {@link DemuxReceiver} taken from this mount,
     * now or later, throws {@link org.tstrans.RtpException} of kind {@code CLOSED}
     * from its next read. PLAY readers and the publisher are unaffected. Idempotent.
     */
    public void cancel() { nCancel(requireOpen(CLOSED)); }

    /** {@link #intoDemuxReceiver(DemuxerConfig)} with the default demuxer configuration. */
    public DemuxReceiver intoDemuxReceiver() throws RtspException {
        long h = nIntoDemuxReceiver(requireOpen(CLOSED),
            false, 0, 0L, 0L, false, 0, 0L, false, 0L, false);
        return wrap(h);
    }

    /**
     * Take the mount's received MPEG-TS as a {@link DemuxReceiver}.
     *
     * <p>Take-once across every handle to the mount (including the one {@link
     * RtspServer#nextPublisher(long)} returned for it): a second call throws
     * {@link RtspException} of kind {@code CLOSED}. This wrapper is not consumed.
     *
     * <p>The receiver outlives publisher churn: between publishers it stays open and
     * silent. Once the mount is removed ({@link RtspServer#removeMount(String)}) or
     * the server stops ({@link RtspServer#stop()} / {@link RtspServer#close()}),
     * {@link DemuxReceiver#recvEvent()} returns {@code null} (end of stream; what
     * was already queued is delivered first). After {@link #cancel()} or {@link
     * DemuxReceiver#close()} a parked read throws
     * {@link org.tstrans.RtpException} of kind {@code CLOSED}.
     *
     * @param demuxConfig demuxer configuration (must not be null; use {@link
     *     #intoDemuxReceiver()} for the defaults). Checked before the take, so a bad
     *     configuration does not spend it.
     * @throws RtspException {@code CLOSED} if the transport was already taken
     */
    public DemuxReceiver intoDemuxReceiver(DemuxerConfig demuxConfig) throws RtspException {
        long h = nIntoDemuxReceiver(requireOpen(CLOSED), true,
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
        return wrap(h);
    }

    /** Free this wrapper only; the mount stays registered in the server. Idempotent. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static final String CLOSED = "PublishMount is closed";

    private static DemuxReceiver wrap(long h) throws RtspException {
        if (h == 0) {
            throw new RtspException(RtspException.Kind.CLOSED,
                "nIntoDemuxReceiver returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    private static native String nMountPath(long handle);
    private static native long nPeerCount(long handle);
    private static native long nGeneration(long handle);
    private static native PublisherInfo nPublisher(long handle);
    private static native PublishMountStats nStats(long handle);
    private static native void nCancel(long handle);
    private static native long nIntoDemuxReceiver(long handle, boolean withConfig,
        int strict, long pesCapPerPid, long pesCapTotal, boolean cfi, int av1,
        long auCellCap, boolean lenientPsi, long syncBufCap, boolean unwrapTimestamps)
        throws RtspException;
    private static native void nClose(long handle);
}
