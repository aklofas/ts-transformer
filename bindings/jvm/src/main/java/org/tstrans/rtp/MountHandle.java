package org.tstrans.rtp;

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;
import org.tstrans.mpegts.AudioStreamHandle;
import org.tstrans.mpegts.DataStreamHandle;
import org.tstrans.mpegts.KlvStreamHandle;
import org.tstrans.mpegts.SubtitleStreamHandle;
import org.tstrans.mpegts.VideoStreamHandle;

/**
 * Push surface for one RTSP-server mount, returned by
 * {@link RtspServer#addUnicastMount} / {@link RtspServer#addMulticastMount}.
 * Mirrors tst-py {@code tstrans.rtp.MountHandle}.
 *
 * <p><b>Thread safety:</b> the underlying Rust handle is {@code Arc}-shared and all
 * {@code push*} calls take {@code &self}, so a single {@code MountHandle} may be used
 * concurrently from multiple threads, including a concurrent {@link #close()}:
 * {@code close()} claims the registry id atomically and the leased
 * {@code HandleRegistry} guarantees no use-after-free/double-free — a push that
 * races a {@code close()} either completes or throws a clean
 * {@link IllegalStateException}.
 *
 * <p><b>Closing:</b> {@code MountHandle} is {@link AutoCloseable} as a JVM-only
 * lifecycle convenience (tst-py relies on Python GC; the JVM has no refcount-driven
 * native free). {@link #close()} frees ONLY this handle wrapper — the mount itself
 * persists in the server (and keeps fanning out) until {@link RtspServer#stop()} /
 * {@link RtspServer#close()}.
 *
 * <p><b>Push errors</b> surface as {@link RtspException} of kind {@code MOUNT}
 * (the failure originates in the mount push path) — this differs from
 * {@link MuxSender}, whose muxer errors are {@code MuxException}.
 *
 * <p><b>Server failure isolation (rare):</b> {@code MountHandle} objects live in their
 * own registry, independent of their originating {@link RtspServer}. If the server is
 * internally invalidated by a panic in a mutating server operation, previously obtained
 * {@code MountHandle} objects are not immediately closed — their next push operation
 * will fail with a closed-channel error rather than throwing at the moment the server is
 * torn down. This is memory-safe; close retained handles when done to ensure timely
 * cleanup.
 */
public final class MountHandle extends NativeHandle {
    static { NativeLoader.load(); }

    MountHandle(long h) { setHandle(h); }

    // ── Identity / introspection ──────────────────────────────────────────
    public String mountPath() { ensureOpen("MountHandle is closed"); return nMountPath(peekHandle()); }
    public long peerCount() { ensureOpen("MountHandle is closed"); return nPeerCount(peekHandle()); }
    /** {@code "unicast"} / {@code "multicast"} / {@code "unknown"}. */
    public String mountKind() { ensureOpen("MountHandle is closed"); return nMountKind(peekHandle()); }
    public MountStats stats() { ensureOpen("MountHandle is closed"); return nStats(peekHandle()); }

    // ── Push family — single stream ───────────────────────────────────────
    public void pushVideo(byte[] nal, long pts, boolean keyFrame) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushVideo(peekHandle(), nal, pts, keyFrame);
    }
    public void pushKlv(byte[] klv, long pts, int metadataServiceId) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushKlv(peekHandle(), klv, pts, metadataServiceId);
    }
    public void pushAudio(byte[] frames, long pts) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushAudio(peekHandle(), frames, pts);
    }
    public void pushSubtitle(byte[] payload, long pts) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushSubtitle(peekHandle(), pts, payload);
    }
    /**
     * Push one private-data payload onto the lone configured data stream
     * (pass-through: no AU-cell wrap, no framing — {@code data} lands verbatim as
     * the PES payload). {@code pts} drives PSI/PCR pacing and is written into the
     * PES header only when the stream was configured with {@code carriesPts = true}.
     */
    public void pushData(byte[] data, long pts) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushData(peekHandle(), data, pts);
    }

    // ── Push family — handle-targeted ─────────────────────────────────────
    public void pushVideoTo(VideoStreamHandle h, byte[] nal, long pts, boolean keyFrame)
            throws RtspException {
        ensureOpen("MountHandle is closed"); nPushVideoTo(peekHandle(), h.raw(), nal, pts, keyFrame);
    }
    public void pushKlvTo(KlvStreamHandle h, byte[] klv, long pts, int metadataServiceId)
            throws RtspException {
        ensureOpen("MountHandle is closed"); nPushKlvTo(peekHandle(), h.raw(), klv, pts, metadataServiceId);
    }
    public void pushAudioTo(AudioStreamHandle h, byte[] frames, long pts) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushAudioTo(peekHandle(), h.raw(), frames, pts);
    }
    public void pushSubtitleTo(SubtitleStreamHandle h, byte[] payload, long pts)
            throws RtspException {
        ensureOpen("MountHandle is closed"); nPushSubtitleTo(peekHandle(), h.raw(), pts, payload);
    }
    /**
     * Push one private-data payload to a specific configured data stream. Same
     * pass-through and PTS semantics as {@link #pushData}. An invalid handle
     * raises {@link RtspException} of kind {@code MOUNT}.
     */
    public void pushDataTo(DataStreamHandle h, byte[] data, long pts) throws RtspException {
        ensureOpen("MountHandle is closed"); nPushDataTo(peekHandle(), h.raw(), data, pts);
    }

    // ── Stream-handle accessors (first-of-kind + all-of-kind) ─────────────
    public Optional<VideoStreamHandle> videoHandle() {
        ensureOpen("MountHandle is closed"); long r = nVideoHandle(peekHandle());
        return r < 0 ? Optional.empty() : Optional.of(VideoStreamHandle.fromRaw(r));
    }
    public Optional<KlvStreamHandle> klvHandle() {
        ensureOpen("MountHandle is closed"); long r = nKlvHandle(peekHandle());
        return r < 0 ? Optional.empty() : Optional.of(KlvStreamHandle.fromRaw(r));
    }
    public Optional<AudioStreamHandle> audioHandle() {
        ensureOpen("MountHandle is closed"); long r = nAudioHandle(peekHandle());
        return r < 0 ? Optional.empty() : Optional.of(AudioStreamHandle.fromRaw(r));
    }
    public Optional<SubtitleStreamHandle> subtitleHandle() {
        ensureOpen("MountHandle is closed"); long r = nSubtitleHandle(peekHandle());
        return r < 0 ? Optional.empty() : Optional.of(SubtitleStreamHandle.fromRaw(r));
    }
    public Optional<DataStreamHandle> dataHandle() {
        ensureOpen("MountHandle is closed"); long r = nDataHandle(peekHandle());
        return r < 0 ? Optional.empty() : Optional.of(DataStreamHandle.fromRaw(r));
    }
    public List<VideoStreamHandle> videoHandles() {
        ensureOpen("MountHandle is closed");
        List<VideoStreamHandle> out = new ArrayList<>();
        for (long r : nVideoHandles(peekHandle())) out.add(VideoStreamHandle.fromRaw(r));
        return out;
    }
    public List<KlvStreamHandle> klvHandles() {
        ensureOpen("MountHandle is closed");
        List<KlvStreamHandle> out = new ArrayList<>();
        for (long r : nKlvHandles(peekHandle())) out.add(KlvStreamHandle.fromRaw(r));
        return out;
    }
    public List<AudioStreamHandle> audioHandles() {
        ensureOpen("MountHandle is closed");
        List<AudioStreamHandle> out = new ArrayList<>();
        for (long r : nAudioHandles(peekHandle())) out.add(AudioStreamHandle.fromRaw(r));
        return out;
    }
    public List<SubtitleStreamHandle> subtitleHandles() {
        ensureOpen("MountHandle is closed");
        List<SubtitleStreamHandle> out = new ArrayList<>();
        for (long r : nSubtitleHandles(peekHandle())) out.add(SubtitleStreamHandle.fromRaw(r));
        return out;
    }
    public List<DataStreamHandle> dataHandles() {
        ensureOpen("MountHandle is closed");
        List<DataStreamHandle> out = new ArrayList<>();
        for (long r : nDataHandles(peekHandle())) out.add(DataStreamHandle.fromRaw(r));
        return out;
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────
    /** Drain buffered TS and broadcast to subscribers. Always safe. */
    public void flush() { ensureOpen("MountHandle is closed"); nFlush(peekHandle()); }
    /** Reset all flow counters to zero. */
    public void resetStats() { ensureOpen("MountHandle is closed"); nResetStats(peekHandle()); }

    /** Free this handle wrapper (the mount itself persists in the server). Idempotent. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    /**
     * Test-only: the raw native handle, for routing a panic through the real
     * {@code REGISTRY_MOUNT} in {@code RtspPanicPoisoningTest} (proves the mount
     * mutators are wired to {@code with_mount_poisoning}). Package-private;
     * the panic probes live in {@code org.tstrans.internal} and are built only
     * under the {@code jni-test-hooks} feature.
     */
    long nativeHandleForTest() { return peekHandle(); }

    // --- Natives ---
    private static native String nMountPath(long handle);
    private static native long nPeerCount(long handle);
    private static native String nMountKind(long handle);
    private static native MountStats nStats(long handle);
    private static native void nPushVideo(long handle, byte[] nal, long pts, boolean keyFrame)
        throws RtspException;
    private static native void nPushKlv(long handle, byte[] klv, long pts, int metadataServiceId)
        throws RtspException;
    private static native void nPushAudio(long handle, byte[] frames, long pts) throws RtspException;
    private static native void nPushSubtitle(long handle, long pts, byte[] payload)
        throws RtspException;
    private static native void nPushData(long handle, byte[] data, long pts) throws RtspException;
    private static native void nPushVideoTo(long handle, long streamHandleRaw, byte[] nal, long pts,
        boolean keyFrame) throws RtspException;
    private static native void nPushKlvTo(long handle, long streamHandleRaw, byte[] klv, long pts,
        int metadataServiceId) throws RtspException;
    private static native void nPushAudioTo(long handle, long streamHandleRaw, byte[] frames,
        long pts) throws RtspException;
    private static native void nPushSubtitleTo(long handle, long streamHandleRaw, long pts,
        byte[] payload) throws RtspException;
    private static native void nPushDataTo(long handle, long streamHandleRaw, byte[] data,
        long pts) throws RtspException;
    private static native long nVideoHandle(long handle);
    private static native long nKlvHandle(long handle);
    private static native long nAudioHandle(long handle);
    private static native long nSubtitleHandle(long handle);
    private static native long nDataHandle(long handle);
    private static native long[] nVideoHandles(long handle);
    private static native long[] nKlvHandles(long handle);
    private static native long[] nAudioHandles(long handle);
    private static native long[] nSubtitleHandles(long handle);
    private static native long[] nDataHandles(long handle);
    private static native void nFlush(long handle);
    private static native void nResetStats(long handle);
    private static native void nClose(long handle);
}
