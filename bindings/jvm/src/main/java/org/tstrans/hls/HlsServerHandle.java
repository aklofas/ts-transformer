package org.tstrans.hls;

import java.net.InetSocketAddress;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.internal.NativeAddr;

/**
 * The built-in HTTP(S) server kept serving a finished playlist + its segments.
 * Returned by {@link HlsPublisher#finishServing()}. {@link #shutdown()} (or
 * {@link #close()}, or try-with-resources) stops serving; idempotent. After
 * shutdown the getters throw {@link IllegalStateException}.
 */
public final class HlsServerHandle extends NativeHandle {
    static { NativeLoader.load(); }

    private final InetSocketAddress localAddr;

    HlsServerHandle(long h) {
        setHandle(h);
        this.localAddr = NativeAddr.decode(nLocalAddr(h));
    }

    /** Bound address. */
    public InetSocketAddress localAddr() {
        ensureOpen("HlsServerHandle is closed");
        return localAddr;
    }

    /** Bound TCP port. */
    public int localPort() {
        ensureOpen("HlsServerHandle is closed");
        return localAddr.getPort();
    }

    /** Stop serving and wait up to 2 s for the {@code tst-hls} thread. Idempotent. */
    public void shutdown() { super.close(); }

    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nShutdown(h); }

    @Override public String toString() {
        return peekHandle() == 0 ? "HlsServerHandle(shutdown)" : "HlsServerHandle(serving)";
    }

    private static native byte[] nLocalAddr(long handle);
    private static native void nShutdown(long handle);
}
