package org.tstrans.srt;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.OpenCancel;
import org.tstrans.shells.SocketStats;

/**
 * Auto-reconnect SRT receiver — wraps {@code tst_pipeline::Receiver
 * <ManagedRecvTransport<SrtTransport>>}. On any Broken/Closed event from the
 * inner socket, re-runs bind + accept under the configured {@link ReconnectPolicy}
 * and resumes delivering bytes from the new connection.
 *
 * <p>Constructed via {@link #fromUrl(String)} or {@link #fromUrl(String, ReconnectPolicy)}
 * with a {@code srt://[host]:port?mode=listener} URL. The open blocks in its first
 * accept until a peer connects; {@link #fromUrl(String, ReconnectPolicy, OpenCancel)}
 * takes an {@link OpenCancel} whose {@code cancel()} ends that accept from another
 * thread.
 *
 * <p>{@link #reconnectAttempts()} exposes the total reconnect ATTEMPT count
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
 * <p><b>Stats:</b> {@link #srtStats()} ALWAYS throws {@code SrtException(IO)},
 * as on {@code ManagedSender}. Use {@link #socketStats()} for the 16-field
 * scheme-neutral view.
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
     * @throws SrtException {@code CONFIG_INVALID} if the policy is zero backoff with unlimited
     *     attempts (refused before connecting), or if the URL is malformed or
     *     uses a non-listener mode; {@code BROKEN} on an initial bind or accept failure (including an
     *     accept timeout)
     */
    public static ManagedReceiver fromUrl(String url, ReconnectPolicy policy) throws SrtException {
        return fromUrl(url, policy, null);
    }

    /**
     * Same as {@link #fromUrl(String, ReconnectPolicy)}, under {@code cancel}: its
     * {@link OpenCancel#cancel()} from another thread ends the first accept, and an
     * object already cancelled refuses the open before the URL is parsed; either
     * way this throws {@code SrtException(CLOSED)}. Once this returns,
     * {@link #cancelHandle()} is the receiver's cancel: the {@code OpenCancel} no
     * longer reaches it or its re-accepts (see {@link OpenCancel} for every rule).
     *
     * @param url    {@code srt://[host]:port?mode=listener[&key=value&...]}
     * @param policy reconnect tuning; {@code null} applies the defaults
     * @param cancel the open's cancel; {@code null} is the same as
     *               {@link #fromUrl(String, ReconnectPolicy)}
     * @return a connected {@code ManagedReceiver}
     * @throws IllegalStateException if {@code cancel} is closed
     * @throws SrtException as in {@link #fromUrl(String, ReconnectPolicy)};
     *     {@code CLOSED} when {@code cancel} was cancelled by the time the open finished
     */
    public static ManagedReceiver fromUrl(String url, ReconnectPolicy policy, OpenCancel cancel)
            throws SrtException {
        PolicyArgs p = PolicyArgs.from(policy);
        long h = nFromUrl(
            url,
            p.maxAttemptsPresent(), p.maxAttempts(),
            p.backoffKind(), p.backoffBaseMs(), p.backoffMaxMs(),
            p.gapBufferCapacity(), p.overflowPolicy(), p.mode(), cancel);
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
     * <p>Each call returns exactly one TS-packet quantum.
     *
     * <p>After {@code cancelHandle().cancel()} this throws {@code SrtException}
     * ({@code CLOSED}), buffered data included.
     *
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code BROKEN} if the transport is broken past the
     *     reconnect budget; {@code IO} on other errors
     */
    public byte[] recvBytes() throws SrtException {
        ensureOpen("ManagedReceiver is closed");
        byte[] result = nRecvBytes(peekHandle());
        if (result == null) {
            throw new SrtException(SrtException.Kind.IO, "nRecvBytes returned null without throwing");
        }
        return result;
    }

    /**
     * {@link #recvBytes()}: {@code maxLen} is ignored, one 188-byte packet per call.
     *
     * @param maxLen ignored
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException {@code BROKEN} or {@code IO} on transport failure
     * @deprecated the argument does nothing; call {@link #recvBytes()}. Removed in 0.12.0.
     */
    @Deprecated(since = "0.11.0", forRemoval = true)
    public byte[] recvBytes(int maxLen) throws SrtException {
        ensureOpen("ManagedReceiver is closed");
        byte[] result = nRecvBytes(peekHandle());
        if (result == null) {
            throw new SrtException(SrtException.Kind.IO, "nRecvBytes returned null without throwing");
        }
        return result;
    }

    /**
     * Total number of reconnect ATTEMPTS (factory invocations) since
     * construction, whether or not they succeeded — the same counter
     * {@link ManagedDemuxReceiver#reconnectAttempts()} reports. Does NOT
     * include the initial bind+accept (which happened in {@link #fromUrl}).
     *
     * <p>Answers without waiting for a parked {@code recvBytes()}, including one
     * parked in a listener-mode re-accept that has not completed — that accept
     * IS an attempt and is counted here while the success counter is still
     * behind it.
     *
     * <p>The recv side has no {@link ManagedTransportStats} record yet (that
     * accessor exists only on the send-side {@link ManagedSender} /
     * {@link ManagedMuxSender}) — this counter remains the only reconnect
     * telemetry available here.
     *
     * <p>Before 0.7.0 this was the successful-rebuild count.
     *
     * @return the reconnect-attempt counter
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
        return ShellAdapters.cancelHandle(ch, "ManagedReceiver");
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
     * SRT-specific 17-field stats — <b>NOT available on a managed receiver</b>.
     * This method ALWAYS throws {@code SrtException(IO)}: {@code ManagedRecvTransport}
     * has no SRT-specific stats accessor. Use {@link #socketStats()} instead.
     *
     * @return never returns normally
     * @throws IllegalStateException if the receiver is closed
     * @throws SrtException always ({@code IO})
     * @deprecated it always throws; call {@link #socketStats()}. Removed in 0.12.0.
     */
    @Deprecated(since = "0.11.0", forRemoval = true)
    public SrtStats srtStats() throws SrtException {
        ensureOpen("ManagedReceiver is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Close the receiver. Cancels first — fires the same cancel target
     * {@link #cancelHandle()} hands out, so any in-flight reconnect exits and a
     * {@link #recvBytes} parked on another thread ends promptly with
     * {@code SrtException(CLOSED)} — then tears down the inner shell.
     * Idempotent. tst-py's {@code close()} has the same contract. The C ABI's
     * {@code tst_managed_receiver_close} does not: there the caller cancels,
     * waits for the parked call to return, then closes.
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
        int gapBufferCapacity, int overflowPolicy, int mode, OpenCancel cancel)
        throws SrtException;
    private static native byte[]  nRecvBytes(long handle) throws SrtException;
    private static native long    nReconnectAttempts(long handle);
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws SrtException;
    private static native void    nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
