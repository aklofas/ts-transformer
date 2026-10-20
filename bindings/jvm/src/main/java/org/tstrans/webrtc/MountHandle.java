package org.tstrans.webrtc;

import java.util.List;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.WebrtcException;

/**
 * Push surface for one {@link WhepServer} mount, returned by
 * {@link WhepServer#addMount}. Mirrors {@code tst_webrtc::MountHandle}.
 *
 * <p>An {@link MountKind#ELEMENTARY} mount takes {@link #pushVideo},
 * {@link #pushKlv} and {@link #pushAudio}; an {@link MountKind#MP2T} mount takes
 * {@link #pushTs}. A push of the other family throws {@code WebrtcException(PUSH)}.
 * No push blocks: a full media queue drops the frame and counts it in
 * {@link #stats()}.
 *
 * <p><b>Thread safety:</b> every push takes the mount by shared reference, so one
 * {@code MountHandle} may be used from several threads at once, including a
 * concurrent {@link #close()}: a push racing it either completes or throws
 * {@link IllegalStateException}.
 *
 * <p><b>Closing:</b> {@link #close()} frees only this handle object; the mount stays
 * on the server and keeps serving its viewers until
 * {@link WhepServer#removeMount(String)}, {@link WhepServer#stop()} or
 * {@link WhepServer#close()}. After those, pushes throw {@code CLOSED}.
 */
public final class MountHandle extends NativeHandle {
    static { NativeLoader.load(); }

    MountHandle(long h) { setHandle(h); }

    /**
     * Push one Annex-B video access unit with its 90 kHz PTS. The NAL units decide
     * whether the frame is a keyframe; {@code keyFrame} is advisory and a
     * disagreement is counted in {@link MountStats#keyframeFlagMismatch()}. KLV
     * records waiting with a PTS at or before {@code pts} are bound to this frame.
     *
     * @throws WebrtcException {@code PUSH} when {@code au} holds no NAL unit or the
     *     mount is MP2T; {@code CLOSED} once the mount is closed
     */
    public void pushVideo(byte[] au, long pts, boolean keyFrame) throws WebrtcException {
        ensureOpen(); nPushVideo(peekHandle(), au, pts, keyFrame);
    }

    /**
     * Push one KLV record (a whole local set, unwrapped) at its 90 kHz PTS; it rides
     * the first frame pushed at or after that PTS.
     *
     * @param metadataServiceId 0 to 255
     * @throws IllegalArgumentException if {@code metadataServiceId} is outside 0..255
     * @throws WebrtcException {@code PUSH} when the record exceeds the 256 KiB
     *     data-channel limit or the mount is MP2T; {@code CLOSED} once the mount is
     *     closed
     */
    public void pushKlv(byte[] record, long pts, int metadataServiceId) throws WebrtcException {
        ensureOpen(); nPushKlv(peekHandle(), record, pts, metadataServiceId);
    }

    /**
     * Push one audio frame at its 90 kHz PTS: under
     * {@link AudioPolicy#PASSTHROUGH_OPUS} one Opus packet, under
     * {@link AudioPolicy#DROP} counted and discarded.
     *
     * @throws WebrtcException {@code PUSH} for an unusable frame or an MP2T mount;
     *     {@code CLOSED} once the mount is closed
     */
    public void pushAudio(byte[] frames, long pts) throws WebrtcException {
        ensureOpen(); nPushAudio(peekHandle(), frames, pts);
    }

    /**
     * Feed MPEG-TS bytes to an MP2T mount, in any chunking: the mount demultiplexes
     * them into its video and KLV paths. The demuxer emits an access unit when the
     * next one starts, so the mount runs one frame behind the bytes pushed and the
     * last access unit waits until more bytes arrive: a file's last access unit is
     * never played.
     *
     * @throws WebrtcException {@code PUSH} on an elementary mount; {@code CLOSED} once
     *     the mount is closed
     */
    public void pushTs(byte[] bytes) throws WebrtcException {
        ensureOpen(); nPushTs(peekHandle(), bytes);
    }

    /** The mount's counters. @throws IllegalStateException if closed. */
    public MountStats stats() { ensureOpen(); return nStats(peekHandle()); }

    /** One entry per viewer session of the mount. @throws IllegalStateException if closed. */
    public List<SessionStats> sessions() {
        ensureOpen(); return List.copyOf(nSessions(peekHandle()));
    }

    /** The mount's path. @throws IllegalStateException if closed. */
    public String mountPath() { ensureOpen(); return nMountPath(peekHandle()); }

    /** Free this handle object; the mount stays on the server. Idempotent. */
    @Override public void close() { super.close(); }

    private void ensureOpen() { ensureOpen("MountHandle is closed"); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native void nPushVideo(long handle, byte[] au, long pts, boolean keyFrame)
        throws WebrtcException;
    private static native void nPushKlv(long handle, byte[] record, long pts,
        int metadataServiceId) throws WebrtcException;
    private static native void nPushAudio(long handle, byte[] frames, long pts)
        throws WebrtcException;
    private static native void nPushTs(long handle, byte[] bytes) throws WebrtcException;
    private static native MountStats nStats(long handle);
    private static native List<SessionStats> nSessions(long handle);
    private static native String nMountPath(long handle);
    private static native void nClose(long handle);
}
