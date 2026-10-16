package org.tstrans.rtp;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;

/**
 * Raw RTP transport (sending side) — wraps {@code tst_rtp::RtpTransport}. Constructed via
 * {@link #fromUrl(String)} with an {@code rtp://host:port[?ttl=&iface=]} URL.
 * Pre-muxed TS bytes are pushed via {@link #send(byte[])} (one UDP datagram per
 * call, framed with an RTP header).
 *
 * <p><b>Thread safety:</b> a single {@code Transport} is NOT thread-safe; use one
 * per thread. <b>Closing:</b> use try-with-resources or {@link #close()};
 * after close, further calls throw {@code IllegalStateException}.
 *
 * <p>Mirrors {@code tstrans.rtp.Transport} in tst-py. The TS shell over an
 * {@code rtp://} URL is {@link org.tstrans.shells.Sender}.
 */
public class Transport extends NativeHandle {
    static { NativeLoader.load(); }

    /** Default UDP datagram size (RTP header + TS payload); matches tst-py. */
    public static final int DEFAULT_PKT_SIZE = 1316;

    /** Wrap a native handle returned by {@link #openHandle(String, int, Long)}. */
    protected Transport(long h) { setHandle(h); }

    /** Construct a transport bound to {@code url} with default packet size and a random SSRC. */
    public static Transport fromUrl(String url) throws RtpException {
        return fromUrl(url, DEFAULT_PKT_SIZE, null);
    }

    /**
     * Construct a transport bound to {@code url}.
     *
     * @param url     {@code rtp://host:port[?key=value&...]}
     * @param pktSize UDP datagram size (RTP header + TS payload); must be &ge; 0
     * @param ssrc    RTP synchronization source identifier (unsigned 32-bit), or
     *                {@code null} to let the transport pick a random one
     * @throws RtpException one member per open-path cause: {@code URL} on a
     *     malformed URL or a rejected URL parameter, {@code PAYLOAD_TYPE_PARAM}
     *     for a {@code ?pt=} on an MPEG-TS sender, {@code HOST_NOT_LITERAL} if
     *     the host is not a literal address, {@code IFACE_UNSUPPORTED} for an
     *     unsupported multicast interface, {@code IO} on the socket bind /
     *     connect itself
     * @throws IllegalArgumentException if {@code pktSize} is negative or {@code ssrc} is out of u32 range
     */
    public static Transport fromUrl(String url, int pktSize, Long ssrc) throws RtpException {
        return new Transport(openHandle(url, pktSize, ssrc));
    }

    /**
     * Open the native transport and return its handle, for a subclass's own
     * factory. Same contract as {@link #fromUrl(String, int, Long)}.
     *
     * @return the native handle, to pass to the constructor
     * @throws RtpException as {@link #fromUrl(String, int, Long)}
     * @throws IllegalArgumentException if {@code pktSize} is negative or {@code ssrc} is out of u32 range
     */
    protected static long openHandle(String url, int pktSize, Long ssrc) throws RtpException {
        if (pktSize < 0) throw new IllegalArgumentException("pktSize must be >= 0: " + pktSize);
        long h = nFromUrl(url, pktSize, ssrc);
        if (h == 0) {
            throw new RtpException(RtpException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return h;
    }

    /**
     * Send one chunk of pre-muxed TS bytes over RTP.
     *
     * @throws IllegalStateException if the sender is closed
     * @throws RtpException {@code TOO_LARGE} if the payload exceeds the
     *     datagram cap; {@code CLOSED} if a cancel fired; {@code IO} otherwise
     */
    public void send(byte[] data) throws RtpException {
        ensureOpen("Transport is closed");
        nSend(peekHandle(), data);
    }

    /** Snapshot of wire-level statistics (never null in normal operation). */
    public org.tstrans.shells.SocketStats socketStats() {
        ensureOpen("Transport is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link org.tstrans.shells.CancelHandle#cancel()}
     * wakes a thread parked in {@link #send}; that call throws
     * {@code RtpException(CLOSED)}.
     */
    public org.tstrans.shells.CancelHandle cancelHandle() {
        ensureOpen("Transport is closed");
        return ShellAdapters.cancelHandle(nCancelHandle(peekHandle()));
    }

    /** Close the transport. Idempotent. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long   nFromUrl(String url, int pktSize, Long ssrc) throws RtpException;
    private static native void   nSend(long handle, byte[] data) throws RtpException;
    private static native org.tstrans.shells.SocketStats nSocketStats(long handle);
    private static native long   nCancelHandle(long handle);
    private static native void   nClose(long handle);
}
