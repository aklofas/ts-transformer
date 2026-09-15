package org.tstrans.srt;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * SRT sender — wraps {@code tst_pipeline::Sender<SrtTransport>}.
 *
 * <p>Constructed via {@link #fromUrl(String)} with a {@code srt://host:port?...}
 * URL using {@code mode=caller} (the default). Query parameters apply through
 * {@code UrlOverlay::apply_to_socket} — passphrase, latency, streamid, etc.
 *
 * <p>Pre-muxed TS bytes are pushed via {@link #sendBytes(byte[])}. The sender
 * internally frames bytes into 7-packet (1316-byte) SRT messages; call
 * {@link #flush()} to emit a partial bundle.
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
 * parked in {@link #sendBytes} within ~3-10 ms, causing that call to throw
 * {@code SrtException(BROKEN)} or {@code SrtException(CLOSED)}.
 *
 * <p>Mirrors {@code tstrans.srt.Sender} in tst-py.
 */
public final class Sender extends NativeHandle {
    static { NativeLoader.load(); }

    /** Package-private constructor from a native handle returned by {@link #nFromUrl}. */
    Sender(long h) { setHandle(h); }

    /**
     * Construct a sender by connecting to the given SRT caller-mode URL.
     *
     * <p>The URL must use {@code mode=caller} (the default when omitted).
     * Resolves the host, opens a libsrt socket, applies query-string options,
     * and blocks on the SRT handshake.
     *
     * @param url {@code srt://host:port[?key=value&...]} with {@code mode=caller}
     * @return a connected {@code Sender}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or
     *     uses a non-caller mode; {@code TIMEOUT} on handshake timeout;
     *     {@code CONNECT_FAILED} on refused/rejected/bad-encryption connections
     */
    public static Sender fromUrl(String url) throws SrtException {
        long h = nFromUrl(url);
        if (h == 0) {
            // nFromUrl throws a pending SrtException; JNI framework re-raises it.
            // This line is unreachable in practice, but satisfies the compiler.
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new Sender(h);
    }

    /**
     * Send one chunk of pre-muxed TS bytes over SRT. Blocks until the bytes
     * are accepted by the libsrt send queue. The sender frames the bytes into
     * 7-packet (1316-byte) SRT messages automatically; partial packets are
     * buffered until the next call or {@link #flush()}.
     *
     * @param data TS bytes to send (any length; need not be packet-aligned)
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code WOULD_BLOCK} if the send queue is full (backpressure);
     *     {@code BROKEN} if the transport is broken; {@code IO} on other errors
     */
    public void sendBytes(byte[] data) throws SrtException {
        ensureOpen("Sender is closed");
        nSendBytes(peekHandle(), data);
    }

    /**
     * Flush any buffered partial TS bundle. Use after the last {@link #sendBytes}
     * call in a logical unit to ensure all bytes reach the peer.
     *
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code BROKEN} on transport failure
     */
    public void flush() throws SrtException {
        ensureOpen("Sender is closed");
        nFlush(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * on any handle wakes a thread parked in {@link #sendBytes}; that call
     * returns {@code SrtException(BROKEN)} or {@code SrtException(CLOSED)}.
     *
     * @return a new {@link CancelHandle} whose {@code cancel()} is forwarded to
     *     the shared underlying libsrt socket
     */
    public CancelHandle cancelHandle() {
        ensureOpen("Sender is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot. Matches the abstract
     * {@code SocketStats} shape shared with {@code tstrans.rtp}. For SRT-specific
     * extras use {@link #srtStats()}.
     *
     * <p>Uses {@code unwrap_or_default} internally — a newly-opened socket with
     * no data yet still yields a zeroed snapshot rather than failing.
     *
     * @return the 16-field wire-stats snapshot (never null in normal operation)
     */
    public SocketStats socketStats() {
        ensureOpen("Sender is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific 17-field stats snapshot. Includes RTT, estimated bandwidth,
     * and the symmetric send/recv-side loss split not available in
     * {@link #socketStats()}.
     *
     * @return a {@link SrtStats} snapshot
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code IO} if the underlying
     *     {@code SrtTransport::stats()} call fails
     */
    public SrtStats srtStats() throws SrtException {
        ensureOpen("Sender is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Close the sender. Cancels the socket first, so a {@link #sendBytes} parked
     * on another thread ends with {@code SrtException(BROKEN)} and this call
     * returns promptly; then best-effort flushes any buffered partial bundle and
     * closes the underlying libsrt socket. Idempotent — subsequent calls are
     * no-ops. After close, further {@link #sendBytes}/{@link #flush} calls
     * throw {@code IllegalStateException}.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the sender owns a live transport.
     *
     * @return liveness state of the underlying SRT socket
     */
    public boolean isAlive() {
        if (peekHandle() == 0) return false;
        return nIsAlive(peekHandle());
    }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long    nFromUrl(String url) throws SrtException;
    private static native void    nSendBytes(long handle, byte[] data) throws SrtException;
    private static native void    nFlush(long handle) throws SrtException;
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws SrtException;
    private static native void    nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
