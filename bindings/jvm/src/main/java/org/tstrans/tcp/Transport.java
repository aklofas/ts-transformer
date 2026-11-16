package org.tstrans.tcp;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;

/**
 * One connection accepted by a {@link Listener}. It stays open when the listener
 * closes.
 *
 * <p>Mirrors {@code tstrans.tcp.Transport} in tst-py.
 */
public final class Transport extends NativeHandle {

    static { NativeLoader.load(); }

    /** Package-private: constructed by {@link Listener#accept()} only. */
    Transport(long h) { setHandle(h); }

    /**
     * The peer's address as {@code ip:port}, an IPv6 address in brackets
     * ({@code [::1]:5000}). It is the TCP source address: behind a proxy, the proxy's.
     *
     * @throws IllegalStateException if the connection is closed
     */
    public String peerAddr() {
        ensureOpen("Transport is closed");
        return nPeerAddr(peekHandle());
    }

    /** Close the connection. Idempotent. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native String nPeerAddr(long handle);

    private static native void nClose(long handle);
}
