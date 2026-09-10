package org.tstrans.srt;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * Auto-reconnect SRT receiver — wraps {@code tst_pipeline::Receiver
 * <ManagedRecvTransport<SrtTransport>>}. On any Broken/Closed event from the
 * inner socket, re-runs bind + accept under the configured {@link ReconnectPolicy}
 * and resumes delivering bytes from the new connection.
 *
 * <p>Constructed via {@link #fromUrl(String)} or {@link #fromUrl(String, ReconnectPolicy)}
 * with a {@code srt://[host]:port?mode=listener} URL.
 *
 * <p>{@link #reconnectAttempts()} exposes the total successful reconnect count
 * (does NOT include the initial bind+accept).
 *
 * <p><b>Thread safety:</b> a single {@code ManagedReceiver} is NOT thread-safe.
 * Use one per thread, or guard with external synchronisation. The sanctioned
 * cross-thread calls are {@link #cancelHandle()}'s {@code cancel()} and
 * {@link #close()}, both of which wake a thread parked in {@link #recvBytes}.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Stats drift:</b> {@link #srtStats()} ALWAYS throws {@code SrtException(IO)}
 * today (same drift as {@code ManagedSender}). Use {@link #socketStats()} for
 * the 16-field scheme-neutral view.
 *
 * <p><b>Reconnect mode:</b> {@link ReconnectMode#BACKGROUND} in the supplied
 * {@link ReconnectPolicy} is send-side only. A receiver accepts it structurally
 * (it rides the shared {@link PolicyArgs} flattening) but the Rust side logs a
 * warning and reconnects as {@link ReconnectMode#BLOCKING} regardless.
 *
 * <p>Mirrors {@code tstrans.srt.ManagedReceiver} in tst-py.
 */
public final class ManagedReceiver extends NativeHandle {
    static { NativeLoader.load(); }

    /** Package-private constructor from a native handle returned by {@link #nFromUrl}. */
    ManagedReceiver(long h) { setHandle(h); }

    /**
     * Bind a managed receiver on the given SRT listener-mode URL with the
     * default {@link ReconnectPolicy} and accept the first incoming connection.
     *
     * @param url {@code srt://[host]:port?mode=listener[&key=value&...]}
     * @return a connected {@code ManagedReceiver}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or
     *     uses a non-listener mode; otherwise the initial bind/accept failure
     */
    public static ManagedReceiver fromUrl(String url) throws SrtException {
        return fromUrl(url, null);
    }

    /**
     * Bind a managed receiver on the given SRT listener-mode URL and accept the
     * first incoming connection.
     *
     * <p>The URL must use {@code mode=listener}. An empty host
     * ({@code srt://:7000?mode=listener}) binds to {@code 0.0.0.0}. The
     * {@code policy} (or {@link ReconnectPolicy#defaults()} when {@code null})
     * governs the initial bind+accept and every subsequent reconnect.
     *
     * @param url {@code srt://[host]:port?mode=listener[&key=value&...]}
     * @param policy reconnect tuning; {@code null} applies the defaults
     * @return a connected {@code ManagedReceiver}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or
     *     uses a non-listener mode; {@code BROKEN} on initial bind/accept failure
     */
    public static ManagedReceiver fromUrl(String url, ReconnectPolicy policy) throws SrtException {
        PolicyArgs p = PolicyArgs.from(policy);
        long h = nFromUrl(
            url,
            p.maxAttemptsPresent(), p.maxAttempts(),
            p.backoffKind(), p.backoffBaseMs(), p.backoffMaxMs(),
            p.gapBufferCapacity(), p.overflowPolicy(), p.mode());
        if (h == 0) {
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new ManagedReceiver(h);
    }

    /**
     * Receive one TS packet (188 bytes) from the underlying transport. Blocks
     * until a packet is available. On a Broken peer, an in-line reconnect runs
     * under the policy before delivery resumes.
     *
     * <p>Each call returns exactly one TS-packet quantum; {@code maxLen} is
     * accepted only for API symmetry and does not cap the read.
     *
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code BROKEN} if the transport is broken past the
     *     reconnect budget; {@code IO} on other errors
     */
    public byte[] recvBytes() throws SrtException {
        return recvBytes(1500);
    }

    /**
     * Receive one TS packet from the underlying transport. The {@code maxLen}
     * parameter is accepted for API symmetry with tst-py but does not alter the
     * one-packet-per-call quantum.
     *
     * @param maxLen hint for the maximum bytes to return (currently ignored
     *     beyond accepting the parameter; one 188-byte packet is returned)
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code BROKEN} or {@code IO} on transport failure
     */
    public byte[] recvBytes(int maxLen) throws SrtException {
        ensureOpen("ManagedReceiver is closed");
        byte[] result = nRecvBytes(peekHandle(), maxLen);
        if (result == null) {
            throw new SrtException(SrtException.Kind.IO, "nRecvBytes returned null without throwing");
        }
        return result;
    }

    /**
     * Total number of successful reconnect rebuilds since construction. Does NOT
     * include the initial bind+accept (which happened in {@link #fromUrl}).
     *
     * <p>The recv side has no {@link ManagedTransportStats} record yet (that
     * accessor exists only on the send-side {@link ManagedSender} /
     * {@link ManagedMuxSender}) — this counter remains the only reconnect
     * telemetry available here.
     *
     * @return the successful-reconnect counter
     * @throws IllegalStateException if the receiver is closed
     */
    public long reconnectAttempts() {
        ensureOpen("ManagedReceiver is closed");
        return nReconnectAttempts(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * latches the wrapper's close flag and wakes a thread parked in
     * {@link #recvBytes()}.
     *
     * @return a new {@link CancelHandle}
     */
    public CancelHandle cancelHandle() {
        ensureOpen("ManagedReceiver is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot from the current inner
     * transport.
     *
     * @return the 16-field wire-stats snapshot (never null in normal operation)
     */
    public SocketStats socketStats() {
        ensureOpen("ManagedReceiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific 17-field stats — <b>NOT available on a managed receiver today</b>.
     * This method ALWAYS throws {@code SrtException(IO)}: {@code ManagedRecvTransport}
     * has no SRT-rich stats accessor. Use {@link #socketStats()} instead. A
     * future tst-pipeline accessor will expose this shape.
     *
     * @return never returns normally
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException always ({@code IO}) — documented stats drift
     */
    public SrtStats srtStats() throws SrtException {
        ensureOpen("ManagedReceiver is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Close the receiver. Cancels first — fires the same cancel target
     * {@link #cancelHandle()} hands out, so any in-flight reconnect exits and a
     * {@link #recvBytes} parked on another thread ends promptly with
     * {@code SrtException(CLOSED)} — then tears down the inner shell.
     * Idempotent. Same contract as the C ABI's {@code tst_managed_receiver_close}
     * and tst-py's {@code close()}.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the managed receiver holds a live shell.
     *
     * @return liveness state of the underlying managed transport
     */
    public boolean isAlive() {
        if (peekHandle() == 0) return false;
        return nIsAlive(peekHandle());
    }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native long nFromUrl(
        String url,
        boolean maxAttemptsPresent, int maxAttempts,
        int backoffKind, long backoffBaseMs, long backoffMaxMs,
        int gapBufferCapacity, int overflowPolicy, int mode) throws SrtException;
    private static native byte[]  nRecvBytes(long handle, int maxLen) throws SrtException;
    private static native long    nReconnectAttempts(long handle);
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws SrtException;
    private static native void    nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
