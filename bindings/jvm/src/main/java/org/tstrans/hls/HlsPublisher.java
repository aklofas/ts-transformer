package org.tstrans.hls;

import java.util.Optional;
import org.tstrans.HlsException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;

/**
 * HLS publisher: segments pushed MPEG-TS bytes to disk and serves them (plus a
 * rolling {@code playlist.m3u8}) over a built-in HTTP(S) server. Mirrors
 * {@code tstrans.hls.HlsPublisher}; wraps {@code tst_hls::HlsPublisher}.
 *
 * <p><b>Lifecycle.</b> {@link #finish()} and {@link #finishServing()} consume the
 * publisher; {@code MuxPublisher#withConfigHls} consumes it too. Afterwards every
 * method throws {@link IllegalStateException} (the JVM-wide closed-handle
 * convention; Python raises {@code HlsError(FINISHED)} for the same state).
 * {@link #close()} is the quiet form: finish, errors dropped, idempotent.
 *
 * <p><b>Threads.</b> Pushes serialise on the handle. {@link #localAddr()},
 * {@link #localPort()} and {@link #toString()} answer from a construction-time
 * snapshot and never wait behind a push on another thread. A {@code close()}
 * racing a push completes after that push's segment write.
 */
public final class HlsPublisher extends NativeHandle implements Publisher {
    static { NativeLoader.load(); }

    private final String localAddr; // null = built without a server

    HlsPublisher(long h) {
        setHandle(h);
        this.localAddr = nLocalAddr(h);
    }

    /** A fresh builder. */
    public static HlsPublisherBuilder builder() { return new HlsPublisherBuilder(); }

    /** Package-private: {@link MuxPublisher#withConfigHls} claims the handle before its native. */
    long consumeHandleForShell() { return consumeHandle(); }

    @Override
    public void pushTs(byte[] tsBytes) throws HlsException {
        ensureOpen("HlsPublisher is closed");
        nPushTs(peekHandle(), tsBytes);
    }

    @Override
    public void cutSegment() throws HlsException {
        ensureOpen("HlsPublisher is closed");
        nCutSegment(peekHandle());
    }

    @Override
    public void cutSegmentWithDuration(long mediaDurationUs) throws HlsException {
        ensureOpen("HlsPublisher is closed");
        nCutSegmentWithDuration(peekHandle(), mediaDurationUs);
    }

    /** Flush, write the terminal playlist, tear down the server. Consumes the publisher. */
    @Override
    public void finish() throws HlsException {
        long h = consumeHandle();
        if (h == 0) throw new IllegalStateException("HlsPublisher is closed");
        nFinish(h);
    }

    /**
     * Like {@link #finish()}, but keep the HTTP server serving the terminal
     * playlist + segments until the returned handle is shut down. Consumes the
     * publisher.
     */
    public HlsServerHandle finishServing() throws HlsException {
        long h = consumeHandle();
        if (h == 0) throw new IllegalStateException("HlsPublisher is closed");
        long sh = nFinishServing(h);
        if (sh == 0) throw new HlsException(HlsException.Kind.INTERNAL, "nFinishServing returned 0 without throwing");
        return new HlsServerHandle(sh);
    }

    @Override
    public PublisherStats stats() {
        ensureOpen("HlsPublisher is closed");
        return nStats(peekHandle());
    }

    /** HLS-specific stats. */
    public HlsStats hlsStats() {
        ensureOpen("HlsPublisher is closed");
        return nHlsStats(peekHandle());
    }

    /**
     * Bound server address as {@code "ip:port"}; the library default is
     * {@code 127.0.0.1:8080} when {@link HlsPublisherBuilder#bind} was not called.
     */
    public Optional<String> localAddr() {
        ensureOpen("HlsPublisher is closed");
        return Optional.ofNullable(localAddr);
    }

    /**
     * Bound TCP port; the library default is {@code 8080} when
     * {@link HlsPublisherBuilder#bind} was not called.
     */
    public int localPort() {
        ensureOpen("HlsPublisher is closed");
        if (localAddr == null) return 0;
        return Integer.parseInt(localAddr.substring(localAddr.lastIndexOf(':') + 1));
    }

    /** Current playlist text; {@code isEvent} selects the terminal form. */
    public String renderPlaylist(boolean isEvent) {
        ensureOpen("HlsPublisher is closed");
        return nRenderPlaylist(peekHandle(), isEvent);
    }

    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    @Override public String toString() {
        return peekHandle() == 0 ? "HlsPublisher(finished)" : "HlsPublisher(open)";
    }

    // --- Natives ---
    static native long nBuild(String url, String bind, String outputDir, long segmentDurationMs,
        long maxSegmentDurationMs, int playlistWindow, int mode, String authUser, String authPass,
        String tlsCert, String tlsKey) throws HlsException;
    private static native String nLocalAddr(long handle);
    private static native void nPushTs(long handle, byte[] tsBytes) throws HlsException;
    private static native void nCutSegment(long handle) throws HlsException;
    private static native void nCutSegmentWithDuration(long handle, long mediaDurationUs) throws HlsException;
    private static native void nFinish(long handle) throws HlsException;
    private static native long nFinishServing(long handle) throws HlsException;
    private static native PublisherStats nStats(long handle);
    private static native HlsStats nHlsStats(long handle);
    private static native String nRenderPlaylist(long handle, boolean isEvent);
    private static native void nClose(long handle);
}
