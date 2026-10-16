package org.tstrans.shells;

import org.tstrans.BindingException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.srt.SrtStats;

/**
 * Pre-muxed TS bytes in, any transport out. Opened from any transport's URL
 * ({@code srt://}, {@code rtp://}, {@code udp://}, {@code tcp://}, {@code tcps://},
 * {@code rist://}); a transport failure throws that transport's exception
 * ({@code SrtException}, {@code RtpException}, {@code UdpException},
 * {@code TcpException}, {@code RistException}), each a {@link BindingException}.
 * Per-transport options are URL query keys.
 *
 * <p>Pre-muxed TS bytes are pushed via {@link #sendBytes(byte[])}. The sender
 * frames bytes into 7-packet (1316-byte) messages; call {@link #flush()} to emit
 * a partial bundle.
 *
 * <p><b>Thread safety:</b> a single {@code Sender} is NOT thread-safe. Use one
 * sender per thread, or guard with external synchronisation.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> {@code sendBytes} copies the supplied
 * array across the JNI boundary; a zero-copy path (FFM {@code MemorySegment})
 * is JDK-22+ only and will be added in a future release.
 *
 * <p><b>Cancellation:</b> call {@link #cancelHandle()} to obtain a
 * {@link CancelHandle}; invoking {@code cancel()} on any handle wakes a thread
 * parked in {@link #sendBytes}, causing that call to throw the transport's
 * exception with kind {@code CLOSED}.
 *
 * <p>Mirrors {@code tstrans.shells.Sender} in tst-py.
 */
public class Sender extends NativeHandle {
    static { NativeLoader.load(); }

    /** Wrap a native handle returned by {@link #openHandle(String)}. */
    protected Sender(long handle) { setHandle(handle); }

    /**
     * Open a sender on the transport named by the URL's scheme.
     *
     * <p>The open connects (or binds) before it returns: an {@code srt://} URL
     * must use {@code mode=caller} (the default).
     *
     * @param url {@code scheme://...} with per-transport query options
     * @return a connected {@code Sender}
     * @throws IllegalArgumentException if the scheme is not one of the six above
     * @throws BindingException the transport's exception for a malformed URL or a
     *     connect failure
     */
    public static Sender open(String url) throws BindingException {
        return new Sender(openHandle(url));
    }

    /**
     * Open the native sender and return its handle, for a subclass's own
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
     * Send one chunk of pre-muxed TS bytes. Blocks until the transport accepts
     * the bytes. Partial bundles are buffered until the next call or
     * {@link #flush()}.
     *
     * @param data TS bytes to send (any length; need not be packet-aligned)
     * @throws IllegalStateException if the sender is closed
     * @throws BindingException the transport's exception: {@code BACKPRESSURE}
     *     if the send queue is full (retry), {@code BROKEN} if the transport is
     *     dead, {@code CLOSED} if cancelled, {@code INPUT_MALFORMED} if the TS
     *     framing is lost
     */
    public void sendBytes(byte[] data) throws BindingException {
        ensureOpen("Sender is closed");
        nSendBytes(peekHandle(), data);
    }

    /**
     * Flush any buffered partial TS bundle. Use after the last {@link #sendBytes}
     * call in a logical unit to ensure all bytes reach the peer.
     *
     * @throws IllegalStateException if the sender is closed
     * @throws BindingException the transport's exception on failure
     */
    public void flush() throws BindingException {
        ensureOpen("Sender is closed");
        nFlush(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * on any handle wakes a thread parked in {@link #sendBytes}; that call
     * throws the transport's exception with kind {@code CLOSED}.
     *
     * @return a new {@link CancelHandle}
     * @throws IllegalStateException if the sender is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("Sender is closed");
        return new CancelHandle(nCancelHandle(peekHandle()));
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot, the same on every transport.
     * A newly-opened transport with no traffic yet yields a zeroed snapshot.
     *
     * @return the wire-stats snapshot
     * @throws IllegalStateException if the sender is closed
     */
    public SocketStats socketStats() {
        ensureOpen("Sender is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific stats snapshot (RTT, estimated bandwidth, symmetric loss
     * split).
     *
     * @return the SRT stats
     * @throws IllegalStateException if the sender is closed
     * @throws UnsupportedOperationException unless the sender was opened from an
     *     {@code srt://} URL
     * @throws BindingException {@code SrtException} if the stats call fails
     */
    public SrtStats srtStats() throws BindingException {
        ensureOpen("Sender is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Close the sender. Cancels first, so a {@link #sendBytes} parked on another
     * thread ends with the transport's {@code CLOSED} and this call returns
     * promptly; a partial bundle in the framing buffer is not delivered, so
     * {@link #flush()} first when the tail matters. Idempotent. After close,
     * further calls throw {@code IllegalStateException}.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the sender owns a live transport. A send in
     * flight on another thread counts as live.
     *
     * @return liveness state of the underlying transport
     */
    public boolean isAlive() {
        long h = peekHandle();
        return h != 0 && nIsAlive(h);
    }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long    nOpen(String url) throws BindingException;
    private static native void    nSendBytes(long handle, byte[] data) throws BindingException;
    private static native void    nFlush(long handle) throws BindingException;
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws BindingException;
    private static native void    nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
