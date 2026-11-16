package org.tstrans.tcp;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.TcpException;
import org.tstrans.shells.CancelHandle;

/**
 * A bound TCP listener ({@code tcp://host:port?listen=1}, or
 * {@code tcps://host:port?listen=1&cert=...&key=...} to serve TLS). Each
 * {@link #accept()} returns one connection as a {@link Transport}.
 *
 * <pre>{@code
 * try (Listener listener = Listener.open("tcp://0.0.0.0:0?listen=1")) {
 *     int port = listener.localPort();
 *     try (Transport conn = listener.accept()) {
 *         System.out.println("publisher at " + conn.peerAddr());
 *     }
 * }
 * }</pre>
 *
 * <p>Mirrors {@code tstrans.tcp.Listener} in tst-py.
 */
public final class Listener extends NativeHandle {

    static { NativeLoader.load(); }

    private Listener(long h) { setHandle(h); }

    /**
     * Bind a listener.
     *
     * @param url {@code tcp://ip:port?listen=1}; the host must be an IP literal, and port
     *     0 lets the kernel pick one (read it back with {@link #localPort()})
     * @return a bound listener
     * @throws TcpException {@code URL} for a malformed URL, one without {@code ?listen=1}
     *     or one whose host is not an IP literal; {@code IO} when the bind fails (the port
     *     is in use, for one); {@code INVALID_CONFIG} for {@code tcps://} without
     *     {@code cert} and {@code key}
     */
    public static Listener open(String url) throws TcpException {
        return new Listener(nOpen(url));
    }

    /**
     * Block until a peer connects and return its connection.
     *
     * <p>One connection is returned per call; connections not yet accepted wait in the
     * kernel's queue, and closing the listener drops them. A peer that connects and then
     * sends nothing is bounded only by a cancel or a close of the shell made from its
     * connection. The peer address is the TCP source address: behind a proxy, the proxy's.
     *
     * @throws TcpException {@code CLOSED} after a cancel, or when a cancel or a
     *     {@link #close()} lands while this call is parked; {@code IO} on an accept
     *     failure; {@code TLS} when a {@code tcps://} handshake fails
     * @throws IllegalStateException if the listener is closed
     */
    public Transport accept() throws TcpException {
        ensureOpen("Listener is closed");
        return new Transport(nAccept(peekHandle()));
    }

    /**
     * The port the listener bound. Read from what was captured at open, so it answers
     * while {@link #accept()} is parked on another thread.
     *
     * @throws TcpException {@code IO} if the bound address could not be read at open
     * @throws IllegalStateException if the listener is closed
     */
    public int localPort() throws TcpException {
        ensureOpen("Listener is closed");
        return nLocalPort(peekHandle());
    }

    /**
     * A handle whose {@link CancelHandle#cancel()} ends a parked {@link #accept()} with
     * {@code TcpException(CLOSED)} from any thread; every later {@code accept()} throws the
     * same. It does not free the listener: {@link #close()} still does.
     *
     * @throws IllegalStateException if the listener is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("Listener is closed");
        return ShellAdapters.cancelHandle(nCancelHandle(peekHandle()));
    }

    /**
     * Close the listener. Cancels first, so an {@link #accept()} parked on another thread
     * ends with {@code TcpException(CLOSED)}, then frees it. Idempotent; connections
     * already accepted stay open.
     */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long nOpen(String url) throws TcpException;

    private static native long nAccept(long handle) throws TcpException;

    private static native int nLocalPort(long handle) throws TcpException;

    private static native long nCancelHandle(long handle);

    private static native void nClose(long handle);
}
