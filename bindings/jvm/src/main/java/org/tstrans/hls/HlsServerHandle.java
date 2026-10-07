package org.tstrans.hls;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;

/**
 * The built-in HTTP(S) server kept serving a finished playlist + its segments.
 * Returned by {@link HlsPublisher#finishServing()}. {@link #shutdown()} (or
 * {@link #close()}, or try-with-resources) stops serving; idempotent. After
 * shutdown the getters throw {@link IllegalStateException}.
 */
public final class HlsServerHandle extends NativeHandle {
    static { NativeLoader.load(); }

    private final String localAddr;

    HlsServerHandle(long h) {
        setHandle(h);
        this.localAddr = nLocalAddr(h);
    }

    /** Bound address as {@code "ip:port"}. */
    public String localAddr() {
        ensureOpen("HlsServerHandle is closed");
        return localAddr;
    }

    /** Bound TCP port. */
    public int localPort() {
        ensureOpen("HlsServerHandle is closed");
        return Integer.parseInt(localAddr.substring(localAddr.lastIndexOf(':') + 1));
    }

    /** Stop serving and drain the runtime. Idempotent. */
    public void shutdown() { super.close(); }

    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nShutdown(h); }

    @Override public String toString() {
        return peekHandle() == 0 ? "HlsServerHandle(shutdown)" : "HlsServerHandle(serving)";
    }

    private static native String nLocalAddr(long handle);
    private static native void nShutdown(long handle);
}
