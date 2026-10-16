package org.tstrans.shells;

import org.tstrans.BindingException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.srt.SrtStats;

/**
 * Any transport in, TS bytes out. Opened from any transport's URL
 * ({@code srt://}, {@code rtp://}, {@code udp://}, {@code tcp://}, {@code tcps://},
 * {@code rist://}); a transport failure throws that transport's exception
 * ({@code SrtException}, {@code RtpException}, {@code UdpException},
 * {@code TcpException}, {@code RistException}), each a {@link BindingException}.
 *
 * <p>An {@code srt://} URL must carry {@code mode=listener}: the open binds,
 * listens and accepts ONE peer, and that first accept is not cancellable because
 * no handle exists until the open returns. For a server that handles many
 * peers, use {@code org.tstrans.srt.Listener}.
 *
 * <p><b>Thread safety:</b> a single {@code Receiver} is NOT thread-safe. Use one
 * receiver per thread, or guard with external synchronisation.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> {@link #recvBytes()} returns a heap
 * {@code byte[]} copy of the received packet; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only.
 *
 * <p><b>Receive quantum:</b> each {@link #recvBytes()} call returns one 188-byte
 * TS packet. The {@code maxLen} parameter is accepted for API symmetry with
 * tst-py and does not cause additional buffering.
 *
 * <p>Mirrors {@code tstrans.shells.Receiver} in tst-py.
 */
public class Receiver extends NativeHandle {
    static { NativeLoader.load(); }

    // The end reason as it stood when close() ran; the native handle is gone by
    // then. Written once by nativeClose, read by endReason()/endDetail().
    private volatile EndReason closedEndReason;
    private volatile String closedEndDetail;

    /** Wrap a native handle returned by {@link #openHandle(String)}. */
    protected Receiver(long handle) { setHandle(handle); }

    /**
     * Open a receiver on the transport named by the URL's scheme.
     *
     * @param url {@code scheme://...} with per-transport query options
     * @return an open {@code Receiver}
     * @throws IllegalArgumentException if the scheme is not one of the six above
     * @throws BindingException the transport's exception for a malformed URL or a
     *     bind/connect fault. An {@code srt://} bind or accept fault is
     *     {@code SrtException(BROKEN)} with the message prefixed {@code bind: }
     *     or {@code accept: }.
     */
    public static Receiver open(String url) throws BindingException {
        return new Receiver(openHandle(url));
    }

    /**
     * Open the native receiver and return its handle, for a subclass's own
     * factory. Same contract as {@link #open(String)}.
     *
     * @param url {@code scheme://...} with per-transport query options
     * @return the native handle, to pass to the constructor
     * @throws IllegalArgumentException if the scheme is not supported
     * @throws BindingException the transport's exception
     */
    protected static long openHandle(String url) throws BindingException {
        return nOpen(url);
    }

    /**
     * Receive one TS packet. Blocks until a packet is available. Defaults
     * {@code maxLen} to 1500.
     *
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws BindingException the transport's exception: {@code END_OF_STREAM}
     *     when the peer closed cleanly, {@code BROKEN} if the transport is dead,
     *     {@code CLOSED} if cancelled
     */
    public byte[] recvBytes() throws BindingException {
        return recvBytes(1500);
    }

    /**
     * Receive one TS packet. Blocks until a packet is available. The
     * {@code maxLen} parameter is accepted for API symmetry and does not alter
     * the one-packet-per-call quantum.
     *
     * @param maxLen hint for the maximum bytes to return (ignored)
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws BindingException the transport's exception
     */
    public byte[] recvBytes(int maxLen) throws BindingException {
        ensureOpen("Receiver is closed");
        return nRecvBytes(peekHandle(), maxLen);
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * on any handle wakes a thread parked in {@link #recvBytes()}; that call
     * throws the transport's exception with kind {@code CLOSED}.
     *
     * @return a new {@link CancelHandle}
     * @throws IllegalStateException if the receiver is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("Receiver is closed");
        return new CancelHandle(nCancelHandle(peekHandle()));
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot, the same on every transport.
     *
     * @return the wire-stats snapshot
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("Receiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific stats snapshot.
     *
     * @return the SRT stats
     * @throws IllegalStateException if the receiver is closed
     * @throws UnsupportedOperationException unless the receiver was opened from
     *     an {@code srt://} URL
     * @throws BindingException {@code SrtException} if the stats call fails
     */
    public SrtStats srtStats() throws BindingException {
        ensureOpen("Receiver is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Why the receive session ended, or {@code null} while it has not ended (or
     * the transport records no end reason). Lock-free, so it answers while
     * {@link #recvBytes()} is parked on another thread; once the receiver is
     * closed it returns the reason as it stood after close.
     *
     * @return the end reason, or {@code null}
     */
    public EndReason endReason() {
        long h = peekHandle();
        if (h == 0) return closedEndReason;
        return EndReason.fromWireValue(nEndReason(h));
    }

    /**
     * Free-text detail for {@link #endReason()}: the message carried by
     * {@code KEEPALIVE_FAILED}, {@code TRANSPORT_FAILED} and
     * {@code PROTOCOL_ERROR}; {@code null} otherwise.
     *
     * @return the detail, or {@code null}
     */
    public String endDetail() {
        long h = peekHandle();
        if (h == 0) return closedEndDetail;
        return nEndDetail(h);
    }

    /**
     * Close the receiver. Cancels first: a {@link #recvBytes()} parked on another
     * thread ends promptly with the transport's {@code CLOSED}, and this call
     * returns without waiting for data. Idempotent. After close, further
     * {@link #recvBytes} calls throw {@code IllegalStateException}.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the receiver owns a live transport. A receive in
     * flight on another thread counts as live.
     *
     * @return liveness state of the underlying transport
     */
    public boolean isAlive() {
        long h = peekHandle();
        return h != 0 && nIsAlive(h);
    }

    @Override protected void nativeClose(long h) {
        EndReason.Snapshot snapshot = nClose(h);
        if (snapshot != null) {
            closedEndDetail = snapshot.detail();
            closedEndReason = snapshot.reason();
        }
    }

    private static native long    nOpen(String url) throws BindingException;
    private static native byte[]  nRecvBytes(long handle, int maxLen) throws BindingException;
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws BindingException;
    private static native int     nEndReason(long handle);
    private static native String  nEndDetail(long handle);
    private static native EndReason.Snapshot nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
