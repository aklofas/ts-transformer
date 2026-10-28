package org.tstrans.webrtc;

import java.util.Objects;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.WebrtcException;

/**
 * WebRTC playback server: browsers pull a mount over WHEP (an HTTP POST of an
 * SDP offer to the mount's path). Construct with {@link #bind(WhepServerConfig)}, add mounts
 * with {@link #addMount(String, MountConfig)} and feed them through the returned
 * {@link MountHandle}. Mirrors {@code tst_webrtc::WhepServer}.
 *
 * <p>The server runs its own two threads (signalling and media); no call on this
 * object parks waiting for the network. Every method is safe to call from any
 * thread, including {@link #close()} while another thread pushes into a mount.
 *
 * <p><b>Closing:</b> {@link #stop()} closes the server and is idempotent: every
 * mount is marked closed (a later push throws {@code WebrtcException(CLOSED)}), the
 * viewers' sessions end, and it waits at most 2 seconds for each server thread.
 * {@link #close()} is {@code stop()} followed by freeing the native server; use
 * try-with-resources. After {@code close()} every method throws
 * {@link IllegalStateException}. {@link MountHandle}s stay valid objects after the
 * server closes; their pushes throw {@code CLOSED}.
 */
public final class WhepServer extends NativeHandle {
    static { NativeLoader.load(); }

    private WhepServer(long h) { setHandle(h); }

    /** {@link #bind(WhepServerConfig)} with {@code new WhepServerConfig(bindUrl)}. */
    public static WhepServer bind(String bindUrl) throws WebrtcException {
        return bind(new WhepServerConfig(bindUrl));
    }

    /**
     * Bind the signalling (HTTP or HTTPS) and media (UDP) ports and start serving.
     *
     * @throws WebrtcException {@code URL} when the URL does not parse (a host name
     *     instead of a literal IP, an unknown query key); {@code TLS} when a
     *     {@code wheps://} certificate or key cannot be loaded; {@code IO} when a port
     *     cannot be bound
     */
    public static WhepServer bind(WhepServerConfig config) throws WebrtcException {
        Objects.requireNonNull(config, "config");
        long h = nBind(config.bindUrl());
        if (h == 0) {
            throw new WebrtcException(WebrtcException.Kind.SERVER,
                "nBind returned 0 without throwing");
        }
        return new WhepServer(h);
    }

    /**
     * Add a mount at {@code path} and return its push handle. A path starts with
     * {@code /}, has no empty segment, holds printable ASCII other than {@code ?}
     * and {@code #}, and its next-to-last segment is not {@code sessions} (that
     * shape names a session). Viewers POST their offer to that path.
     *
     * @throws WebrtcException {@code MOUNT} for an invalid or already mounted path or
     *     a configuration the server refuses (H.266 or AV1 video, an SEI budget
     *     outside 1 to 1 MiB, a viewer queue below 16, Opus passthrough on an MP2T
     *     mount); {@code CLOSED} after {@link #stop()}
     */
    public MountHandle addMount(String path, MountConfig config) throws WebrtcException {
        Objects.requireNonNull(path, "path");
        Objects.requireNonNull(config, "config");
        ensureOpen();
        long h = nAddMount(peekHandle(), path, config.kind().ordinal(),
            config.video().ordinal(), config.audio().ordinal(), config.klv().ordinal(),
            config.seiBudgetBytes(), config.viewerQueuePackets());
        if (h == 0) {
            throw new WebrtcException(WebrtcException.Kind.MOUNT,
                "nAddMount returned 0 without throwing");
        }
        return new MountHandle(h);
    }

    /**
     * Remove the mount at {@code path}: its handles' pushes then throw {@code CLOSED}
     * and its viewers' sessions are closed.
     *
     * @throws WebrtcException {@code MOUNT} when nothing is mounted at {@code path}
     */
    public void removeMount(String path) throws WebrtcException {
        Objects.requireNonNull(path, "path");
        ensureOpen();
        nRemoveMount(peekHandle(), path);
    }

    /**
     * Server-wide counters. The counters live on the media thread, so this call
     * asks it and can wait up to 2 seconds for the reply when that thread is busy;
     * keep it off a thread that paces pushes. @throws IllegalStateException if closed.
     */
    public ServerStats stats() { ensureOpen(); return nStats(peekHandle()); }

    /**
     * The signalling socket's address as {@code "ip:port"}, with the kernel's port
     * when the URL asked for port 0. @throws IllegalStateException if closed.
     */
    public String localAddr() { ensureOpen(); return nLocalAddr(peekHandle()); }

    /**
     * The media (UDP) socket's address as {@code "ip:port"}, shared by every viewer.
     * @throws IllegalStateException if closed.
     */
    public String mediaAddr() { ensureOpen(); return nMediaAddr(peekHandle()); }

    /**
     * Stop serving; see the class's <b>Closing</b> section. Idempotent: a second call
     * returns at once. @throws IllegalStateException if closed.
     */
    public void stop() { ensureOpen(); nStop(peekHandle()); }

    /** {@link #stop()}, then free the native server. Idempotent. */
    @Override public void close() { super.close(); }

    private void ensureOpen() { ensureOpen("WhepServer is closed"); }

    /** Test-only: the raw native handle, for the jni-test-hooks viewer probe. */
    long nativeHandleForTest() { return peekHandle(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long nBind(String bindUrl) throws WebrtcException;
    private static native long nAddMount(long handle, String path, int kind, int video,
        int audio, int klv, int seiBudgetBytes, int viewerQueuePackets) throws WebrtcException;
    private static native void nRemoveMount(long handle, String path) throws WebrtcException;
    private static native ServerStats nStats(long handle);
    private static native String nLocalAddr(long handle);
    private static native String nMediaAddr(long handle);
    private static native void nStop(long handle);
    private static native void nClose(long handle);
}
