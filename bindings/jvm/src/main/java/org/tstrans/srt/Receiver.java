package org.tstrans.srt;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * SRT receiver — wraps {@code tst_pipeline::Receiver<SrtTransport>}.
 *
 * <p>Constructed via {@link #fromUrl(String)} with a {@code srt://host:port?mode=listener}
 * URL. Binds the socket, then blocks on the first incoming SRT handshake
 * (one-shot accept). The accepted socket becomes the receive transport.
 *
 * <p>For a server that handles many peers, use the lower-level
 * {@code Listener} class and accept in a loop.
 *
 * <p><b>Thread safety:</b> a single {@code Receiver} is NOT thread-safe.
 * Use one receiver per thread, or guard with external synchronisation.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> {@link #recvBytes()} returns a heap
 * {@code byte[]} copy of the received packet. A zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 *
 * <p><b>Receive quantum:</b> each {@link #recvBytes()} call returns exactly one
 * 188-byte TS packet — the natural SRT live-mode unit. The {@code maxLen}
 * parameter is accepted for API symmetry with tst-py but does not cause
 * additional buffering.
 *
 * <p><b>Cancellation:</b> call {@link #cancelHandle()} to obtain a
 * {@link CancelHandle}; invoking {@code cancel()} on any handle wakes a thread
 * parked in {@link #recvBytes()} within ~3-10 ms, causing that call to throw
 * {@code SrtException(BROKEN)} or {@code SrtException(CLOSED)}.
 *
 * <p>Mirrors {@code tstrans.srt.Receiver} in tst-py.
 */
public final class Receiver extends NativeHandle {
    static { NativeLoader.load(); }

    /** Package-private constructor from a native handle returned by {@link #nFromUrl}. */
    Receiver(long h) { setHandle(h); }

    /**
     * Bind a receiver listening on the given SRT URL and accept the first
     * incoming connection.
     *
     * <p>The URL must use {@code mode=listener}. An empty host
     * ({@code srt://:7000?mode=listener}) binds to {@code 0.0.0.0}.
     *
     * @param url {@code srt://[host]:port?mode=listener[&key=value&...]}
     * @return a connected {@code Receiver}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or
     *     uses a non-listener mode; {@code TIMEOUT} on accept timeout;
     *     {@code CONNECT_FAILED} if the socket cannot be bound;
     *     {@code ACCEPT_FAILED} on handshake rejection
     */
    public static Receiver fromUrl(String url) throws SrtException {
        long h = nFromUrl(url);
        if (h == 0) {
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new Receiver(h);
    }

    /**
     * Receive one TS packet (188 bytes) from the underlying transport. Blocks
     * until a packet is available. SRT live mode delivers in 188-byte units;
     * a single packet is returned per call (the natural quantum).
     *
     * <p>This overload defaults {@code maxLen} to 1500.
     *
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code BROKEN} if the transport is broken;
     *     {@code IO} on other errors
     */
    public byte[] recvBytes() throws SrtException {
        return recvBytes(1500);
    }

    /**
     * Receive one TS packet from the underlying transport. Blocks until a
     * packet is available. The {@code maxLen} parameter is accepted for API
     * symmetry with tst-py but does not alter the one-packet-per-call quantum.
     *
     * @param maxLen hint for the maximum bytes to return (currently ignored
     *     beyond accepting the parameter; one 188-byte packet is returned)
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code BROKEN} or {@code IO} on transport failure
     */
    public byte[] recvBytes(int maxLen) throws SrtException {
        ensureOpen("Receiver is closed");
        byte[] result = nRecvBytes(peekHandle(), maxLen);
        if (result == null) {
            // nRecvBytes threw a pending SrtException; JNI framework re-raises it.
            throw new SrtException(SrtException.Kind.IO, "nRecvBytes returned null without throwing");
        }
        return result;
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * on any handle wakes a thread parked in {@link #recvBytes()}; that call
     * returns {@code SrtException(BROKEN)} or {@code SrtException(CLOSED)}.
     *
     * @return a new {@link CancelHandle}
     */
    public CancelHandle cancelHandle() {
        ensureOpen("Receiver is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot. For SRT-specific extras use
     * {@link #srtStats()}.
     *
     * @return the 16-field wire-stats snapshot (never null in normal operation)
     */
    public SocketStats socketStats() {
        ensureOpen("Receiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific 17-field stats snapshot.
     *
     * @return a {@link SrtStats} snapshot
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code IO} if the underlying
     *     {@code SrtTransport::stats()} call fails
     */
    public SrtStats srtStats() throws SrtException {
        ensureOpen("Receiver is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Close the receiver. Closes the underlying libsrt socket. Idempotent —
     * subsequent calls are no-ops. After close, further {@link #recvBytes}
     * calls throw {@code IllegalStateException}.
     *
     * <p>If a thread is parked in {@link #recvBytes}, {@code close()} cancels
     * first: it fires the receiver's cancel target before taking the resource
     * lock the parked recv holds, so that call ends promptly with
     * {@code SrtException(BROKEN)} — the cancel closes the libsrt socket under
     * it; the managed {@link ManagedReceiver} surfaces the same close as
     * {@code CLOSED} — and {@code close()} returns without waiting for data.
     * Same contract as the rtp receiver and the Python / C twins.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the receiver owns a live transport.
     *
     * @return liveness state of the underlying SRT socket
     */
    public boolean isAlive() {
        if (peekHandle() == 0) return false;
        return nIsAlive(peekHandle());
    }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long    nFromUrl(String url) throws SrtException;
    private static native byte[]  nRecvBytes(long handle, int maxLen) throws SrtException;
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws SrtException;
    private static native void    nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
