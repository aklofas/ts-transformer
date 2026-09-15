package org.tstrans.srt;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * Auto-reconnect SRT sender — wraps {@code tst_pipeline::Sender
 * <ManagedTransport<SrtTransport>>}. On any Broken/Closed event from the inner
 * socket, the captured {@code mode=caller} URL is rerun through the reconnect
 * factory under the configured {@link ReconnectPolicy} and sending resumes.
 *
 * <p>Constructed via {@link #fromUrl(String)} or {@link #fromUrl(String, ReconnectPolicy)}
 * with a {@code srt://host:port?...} URL using {@code mode=caller} (the default).
 * The supplied policy applies identically to the initial connect and every
 * subsequent reconnect.
 *
 * <p><b>Thread safety:</b> a single {@code ManagedSender} is NOT thread-safe.
 * Use one per thread, or guard with external synchronisation.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Stats drift:</b> {@link #srtStats()} ALWAYS throws {@code SrtException(IO)}
 * today — {@code ManagedTransport} does not expose the SRT-rich 17-field shape
 * (no accessor in tst-pipeline). Use {@link #socketStats()} for the 16-field
 * scheme-neutral view. A future tst-pipeline accessor will lift this.
 *
 * <p>Mirrors {@code tstrans.srt.ManagedSender} in tst-py.
 */
public final class ManagedSender extends NativeHandle {
    static { NativeLoader.load(); }

    /** Package-private constructor from a native handle returned by {@link #nFromUrl}. */
    ManagedSender(long h) { setHandle(h); }

    /**
     * Construct a managed sender by connecting to the given SRT caller-mode URL
     * with the default {@link ReconnectPolicy}.
     *
     * @param url {@code srt://host:port[?key=value&...]} with {@code mode=caller}
     * @return a connected {@code ManagedSender}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or
     *     uses a non-caller mode; otherwise the initial-connect failure kind
     */
    public static ManagedSender fromUrl(String url) throws SrtException {
        return fromUrl(url, null);
    }

    /**
     * Construct a managed sender by connecting to the given SRT caller-mode URL.
     *
     * <p>The URL must use {@code mode=caller} (the default when omitted). The
     * {@code policy} (or {@link ReconnectPolicy#defaults()} when {@code null})
     * governs the initial connect and every subsequent reconnect.
     *
     * @param url {@code srt://host:port[?key=value&...]} with {@code mode=caller}
     * @param policy reconnect tuning; {@code null} applies the defaults
     * @return a connected {@code ManagedSender}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or
     *     uses a non-caller mode; {@code BROKEN} on initial-connect failure
     */
    public static ManagedSender fromUrl(String url, ReconnectPolicy policy) throws SrtException {
        PolicyArgs p = PolicyArgs.from(policy);
        long h = nFromUrl(
            url,
            p.maxAttemptsPresent(), p.maxAttempts(),
            p.backoffKind(), p.backoffBaseMs(), p.backoffMaxMs(),
            p.gapBufferCapacity(), p.overflowPolicy(), p.mode());
        if (h == 0) {
            // nFromUrl throws a pending SrtException; JNI framework re-raises it.
            // This line is unreachable in practice, but satisfies the compiler.
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new ManagedSender(h);
    }

    /**
     * Send one chunk of pre-muxed TS bytes over SRT. Blocks until the bytes are
     * accepted by the libsrt send queue. On a Broken peer, an in-line reconnect
     * runs under the policy before the bytes are retried.
     *
     * @param data TS bytes to send (any length; need not be packet-aligned)
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code WOULD_BLOCK} on backpressure; {@code BROKEN}
     *     if the transport is broken past the reconnect budget; {@code IO} otherwise
     */
    public void sendBytes(byte[] data) throws SrtException {
        ensureOpen("ManagedSender is closed");
        nSendBytes(peekHandle(), data);
    }

    /**
     * Flush any buffered partial TS bundle.
     *
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code BROKEN} on transport failure
     */
    public void flush() throws SrtException {
        ensureOpen("ManagedSender is closed");
        nFlush(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * latches the managed wrapper's close flag (preventing further reconnects)
     * and wakes a thread parked in {@link #sendBytes}.
     *
     * @return a new {@link CancelHandle}
     */
    public CancelHandle cancelHandle() {
        ensureOpen("ManagedSender is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot from the current inner
     * transport. Uses {@code unwrap_or_default} internally, so a mid-reconnect
     * sender yields a zeroed snapshot rather than failing.
     *
     * @return the 16-field wire-stats snapshot (never null in normal operation)
     */
    public SocketStats socketStats() {
        ensureOpen("ManagedSender is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific 17-field stats — <b>NOT available on a managed sender today</b>.
     * This method ALWAYS throws {@code SrtException(IO)}: {@code ManagedTransport}
     * has no SRT-rich stats accessor. Use {@link #socketStats()} instead. A
     * future tst-pipeline accessor will expose this shape.
     *
     * @return never returns normally
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException always ({@code IO}) — documented stats drift
     */
    public SrtStats srtStats() throws SrtException {
        ensureOpen("ManagedSender is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Reconnect/gap telemetry: attempts, successes, current gap-buffer depth, and
     * drop counters. Always readable — unlike {@link #socketStats()} /
     * {@link #srtStats()}, it does not require a live inner transport (the
     * counters live in a side channel that survives reconnect cycles), but it
     * DOES require the sender itself not be closed.
     *
     * <p>{@link ManagedTransportStats#reconnecting()} is only ever {@code true}
     * under {@link ReconnectMode#BACKGROUND}.
     *
     * @return the reconnect/gap telemetry snapshot
     * @throws IllegalStateException if the sender is closed
     * @throws SrtException {@code IO} if the internal gap-buffer lock is poisoned
     */
    public ManagedTransportStats reconnectStats() throws SrtException {
        ensureOpen("ManagedSender is closed");
        return nReconnectStats(peekHandle());
    }

    /**
     * Close the sender. Cancels first — latching the close flag so any in-flight
     * reconnect loop exits and waking a {@link #sendBytes} parked on another
     * thread, which ends with {@code SrtException(CLOSED)} — then tears down
     * the inner transport. Idempotent.
     */
    @Override public void close() { super.close(); }

    /**
     * Return {@code true} while the managed sender holds a live transport.
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
    private static native void    nSendBytes(long handle, byte[] data) throws SrtException;
    private static native void    nFlush(long handle) throws SrtException;
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws SrtException;
    private static native ManagedTransportStats nReconnectStats(long handle) throws SrtException;
    private static native void    nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
