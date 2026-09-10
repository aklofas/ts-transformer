package org.tstrans.srt;

import java.util.Iterator;
import java.util.NoSuchElementException;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * Single-call convenience wrapper that owns a managed-reconnect demuxer over an
 * SRT transport. Construct with an SRT URL — {@code mode=listener} (default) OR
 * {@code mode=caller} — and iterate over the emitted {@link DemuxEvent} instances
 * (the same sealed hierarchy {@code org.tstrans.mpegts.Demuxer} produces). On any
 * Broken/Closed event the wrapper re-binds (listener) or re-dials (caller) under
 * the configured {@link ReconnectPolicy} and resumes delivering events.
 *
 * <p>Mirrors {@code tstrans.srt.ManagedDemuxReceiver}. Wraps
 * {@code tst_pipeline::ManagedDemuxReceiver<SrtTransport>}.
 *
 * <p><b>Reconnect discontinuity:</b> after each transport reconnect the inner
 * receiver emits exactly one {@link DemuxEvent.ReconnectDiscontinuity} before any
 * post-reconnect events. Consumers should drop per-stream caches on receipt and
 * rebuild from the next {@link DemuxEvent.ProgramMap}.
 *
 * <p><b>Mode:</b> unlike the plain {@code DemuxReceiver} (listener only), this
 * wrapper accepts {@code mode=caller} too — in caller mode it dials the peer on
 * each (re)connect; in listener mode it re-binds and re-accepts.
 *
 * <pre>{@code
 * try (ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl("srt://:7000?mode=listener")) {
 *     for (DemuxEvent e : rx) {
 *         if (e instanceof DemuxEvent.ReconnectDiscontinuity) {
 *             // drop caches; rebuild on the next ProgramMap
 *         } else if (e instanceof DemuxEvent.Video v) { ... }
 *     }
 * }
 * }</pre>
 *
 * <p><b>Thread safety:</b> a single {@code ManagedDemuxReceiver} is NOT
 * thread-safe and is deliberately NOT {@code synchronized}. Iterate from one
 * thread; the only sanctioned cross-thread operation is {@link #cancelHandle()}'s
 * {@code cancel()}, which wakes a thread parked in iteration.
 *
 * <p><b>Stats:</b> both {@link #socketStats()} and {@link #srtStats()} return a
 * {@link SocketStats} — {@code srtStats()} returns the SAME value as
 * {@code socketStats()} (a documented drift mirroring tst-py, where
 * {@code ManagedRecvTransport} exposes no separate SRT-rich shape) and does NOT
 * throw. There is NO combined {@code stats()} on this wrapper.
 * {@link #reconnectAttempts()} counts every reconnect-factory invocation since
 * construction (an ATTEMPT counter).
 *
 * <p><b>Reconnect mode:</b> {@link ReconnectMode#BACKGROUND} in the supplied
 * {@link ReconnectPolicy} is send-side only. This receiver accepts it
 * structurally (it rides the shared {@link PolicyArgs} flattening) but the
 * Rust side logs a warning and reconnects as {@link ReconnectMode#BLOCKING}
 * regardless.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> sample payloads deliver heap
 * {@code byte[]} copies; a zero-copy path (FFM {@code MemorySegment}) is JDK-22+
 * only and will be added in a future release.
 */
public final class ManagedDemuxReceiver extends NativeHandle implements Iterable<DemuxEvent> {
    static { NativeLoader.load(); }

    // Populated by nativeClose from nClose's close-time ordinal (see
    // endReason()'s javadoc for why: once closed, peekHandle() is 0 and the
    // native registry entry is gone for good).
    private volatile RecvEndReason closedEndReason;

    /** Package-private constructor from a native handle. */
    ManagedDemuxReceiver(long h) { setHandle(h); }

    /**
     * Bind (or connect) a managed receiver on the given SRT URL with the default
     * {@link ReconnectPolicy} and default demux options.
     *
     * @param url {@code srt://[host]:port?mode=listener|caller[&key=value&...]}
     * @return a connected {@code ManagedDemuxReceiver}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed;
     *     {@code CONNECT_FAILED} on initial bind/connect failure
     */
    public static ManagedDemuxReceiver fromUrl(String url) throws SrtException {
        return fromUrl(url, (ReconnectPolicy) null);
    }

    /**
     * Bind (or connect) a managed receiver on the given SRT URL with default
     * demux options.
     *
     * @param url    {@code srt://[host]:port?mode=listener|caller[&...]}
     * @param policy reconnect tuning; {@code null} applies the defaults
     * @return a connected {@code ManagedDemuxReceiver}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed;
     *     {@code CONNECT_FAILED} on initial bind/connect failure
     */
    public static ManagedDemuxReceiver fromUrl(String url, ReconnectPolicy policy)
            throws SrtException {
        PolicyArgs p = PolicyArgs.from(policy);
        long h = nFromUrl(
            url,
            p.maxAttemptsPresent(), p.maxAttempts(),
            p.backoffKind(), p.backoffBaseMs(), p.backoffMaxMs(),
            p.gapBufferCapacity(), p.overflowPolicy(), p.mode());
        if (h == 0) {
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new ManagedDemuxReceiver(h);
    }

    /**
     * Bind (or connect) a managed receiver on the given SRT URL with an explicit
     * {@link DemuxerConfig} and the default {@link ReconnectPolicy}.
     *
     * @param url         {@code srt://[host]:port?mode=listener|caller[&...]}
     * @param demuxConfig the demuxer configuration
     * @return a connected {@code ManagedDemuxReceiver}
     * @throws SrtException as in {@link #fromUrl(String)}
     */
    public static ManagedDemuxReceiver fromUrl(String url, DemuxerConfig demuxConfig)
            throws SrtException {
        return fromUrl(url, demuxConfig, null);
    }

    /**
     * Bind (or connect) a managed receiver on the given SRT URL with an explicit
     * {@link DemuxerConfig} and reconnect policy.
     *
     * @param url         {@code srt://[host]:port?mode=listener|caller[&...]}
     * @param demuxConfig the demuxer configuration
     * @param policy      reconnect tuning; {@code null} applies the defaults
     * @return a connected {@code ManagedDemuxReceiver}
     * @throws SrtException as in {@link #fromUrl(String)}
     */
    public static ManagedDemuxReceiver fromUrl(String url, DemuxerConfig demuxConfig, ReconnectPolicy policy)
            throws SrtException {
        PolicyArgs p = PolicyArgs.from(policy);
        long h = nFromUrlWithConfig(
            url,
            p.maxAttemptsPresent(), p.maxAttempts(),
            p.backoffKind(), p.backoffBaseMs(), p.backoffMaxMs(),
            p.gapBufferCapacity(), p.overflowPolicy(), p.mode(),
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
        if (h == 0) {
            throw new SrtException(SrtException.Kind.IO,
                "nFromUrlWithConfig returned 0 without throwing");
        }
        return new ManagedDemuxReceiver(h);
    }

    /**
     * Iterate the demuxed events. Each {@code hasNext()} blocks on the next
     * {@code recv_event} until an event arrives, the transport closes cleanly
     * (clean EOF → iteration ends), or an error occurs. Checked
     * {@link SrtException} / {@link DemuxException} surfaced by the native pull
     * are wrapped in an unchecked {@link RuntimeException} (the {@code Iterator}
     * contract forbids checked exceptions). The stream emits exactly one
     * {@link DemuxEvent.ReconnectDiscontinuity} after each transport reconnect.
     */
    @Override
    public Iterator<DemuxEvent> iterator() {
        return new Iterator<>() {
            private DemuxEvent peeked;
            private boolean done;
            @Override public boolean hasNext() {
                if (done) return false;
                if (peeked != null) return true;
                try {
                    peeked = nNext(peekHandle());
                } catch (SrtException | DemuxException e) {
                    throw new RuntimeException(e);
                }
                if (peeked == null) { done = true; return false; }
                return true;
            }
            @Override public DemuxEvent next() {
                if (!hasNext()) throw new NoSuchElementException();
                DemuxEvent e = peeked; peeked = null; return e;
            }
        };
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * wakes a thread parked in iteration — a blocked receive, the reconnect
     * backoff, or a listener-mode re-accept — and that pull then ends or throws.
     *
     * <p>Safe to call from another thread at any time while the receiver is open,
     * including while iteration is parked and while a reconnect is in flight: the
     * cancel target is captured when the receiver is opened and follows the
     * managed transport across reconnects, so this call never waits for an
     * in-flight native receive.
     *
     * @return a new {@link CancelHandle}
     * @throws IllegalStateException if the receiver is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("ManagedDemuxReceiver is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot from the current inner
     * transport (zeros while mid-reconnect).
     *
     * @return the wire-stats snapshot (never null in normal operation)
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("ManagedDemuxReceiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * SRT-specific stats — <b>returns the same {@link SocketStats} view as
     * {@link #socketStats()}</b> (documented drift; {@code ManagedRecvTransport}
     * exposes no separate SRT-rich shape). Unlike {@code ManagedSender}/
     * {@code ManagedReceiver} this does NOT throw, and the return type is
     * {@code SocketStats}, not {@code SrtStats}.
     *
     * @return the wire-stats snapshot (same as {@link #socketStats()})
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats srtStats() {
        ensureOpen("ManagedDemuxReceiver is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Total number of times the reconnect factory has been invoked since
     * construction. {@code 0} means the initial connect is still live; rising
     * values mean the inner SRT transport has been rebuilt (or a rebuild attempt
     * failed and was retried).
     *
     * <p>The recv side has no {@link ManagedTransportStats} record yet (that
     * accessor exists only on the send-side {@link ManagedSender} /
     * {@link ManagedMuxSender}) — this counter remains the only reconnect
     * telemetry available here.
     *
     * @return the reconnect-attempt counter
     * @throws IllegalStateException if the receiver is closed
     */
    public long reconnectAttempts() {
        ensureOpen("ManagedDemuxReceiver is closed");
        return nReconnectAttempts(peekHandle());
    }

    /**
     * Wall-clock time the stream identified by {@code pid} last carried a
     * demuxed item through this receiver (last emitted event), as a
     * Unix-epoch microsecond count. {@code null} if {@code pid} was never
     * seen — including an unrecognized PID (plain {@code int}-to-{@code
     * u16} cast, no range check, same as the {@code pmtPid}/{@code pcrPid}
     * parameters elsewhere in this binding) or before any event has
     * arrived. Unlike {@link #socketStats()}, this reads the demuxer-side
     * counters (not the live transport), so it is unaffected by a
     * mid-reconnect gap.
     *
     * <p><b>Blocking:</b> this takes the same internal resource lock a
     * parked {@code next()} call holds — if an iteration is currently in
     * flight on this receiver, {@code lastSeenMicros} blocks until it
     * returns (an event, end of stream, or an error), which on a
     * fully-quiet stream may be indefinite. Call between {@code next()}
     * calls, or from the same thread driving the iteration, to avoid
     * blocking.
     *
     * @param pid the stream PID to query
     * @return the last-seen timestamp in Unix-epoch microseconds, or
     *     {@code null}
     * @throws IllegalStateException if the receiver is closed
     */
    public Long lastSeenMicros(int pid) {
        ensureOpen("ManagedDemuxReceiver is closed");
        long v = nLastSeenMicros(peekHandle(), pid);
        return v < 0 ? null : v;
    }

    /**
     * Close the receiver. Closes the underlying libsrt socket and stops further
     * reconnects. Idempotent — subsequent calls are no-ops.
     *
     * <p>If a thread is parked in iteration ({@code next()}), {@code close()}
     * blocks until that call returns — it acquires the receiver's resource lock,
     * which the parked recv holds. Unlike the rtp receiver, srt {@code close()}
     * does NOT itself wake a parked recv; to unblock it from another thread, call
     * {@link #cancelHandle()}{@code .cancel()} first.
     */
    @Override public void close() { super.close(); }

    /**
     * Why this receiver's stream ended, or {@code null} while it is still live
     * (or if it ended through a path {@code tst-pipeline} does not instrument).
     *
     * <p>Recorded once, first-writer-wins, by the iteration itself at the moment
     * it observes the terminal condition — so read it <em>after</em> iteration
     * ends to learn why: {@link RecvEndReason#CANCELLED} when
     * {@link #cancelHandle()}{@code .cancel()} or {@link #close()} stopped it,
     * {@link RecvEndReason#RECONNECT_EXHAUSTED} when the
     * {@link ReconnectPolicy} budget ran out. See {@link RecvEndReason} for why
     * {@link RecvEndReason#END_OF_STREAM} does not occur here.
     *
     * <p><b>Non-blocking, unlike the other getters.</b> {@link #socketStats()}
     * and {@link #lastSeenMicros(int)} take the internal resource lock a parked
     * {@code next()} holds and therefore wait for it; this call does not. The
     * end-reason cell is captured when the receiver is opened and read without
     * that lock, so it stays answerable from another thread while iteration is
     * parked — including in a listener-mode re-accept, which may never return on
     * its own.
     *
     * <p><b>After {@link #close()}</b> this keeps returning the recorded reason:
     * {@code close()} captures it in the same native call that tears the
     * receiver down, because the native registry entry is gone afterwards.
     * Unlike every other accessor on this class, it does not throw once closed.
     *
     * <p><b>Cross-thread close race:</b> a concurrent call from another thread
     * while {@link #close()} is in flight may briefly observe {@code null} —
     * {@code close()} claims the handle (so this method switches to the cached
     * snapshot) before the native teardown that computes that snapshot
     * completes. Read it from the thread that closed, or after {@code close()}
     * returns.
     *
     * @return the recorded reason, or {@code null}
     */
    public RecvEndReason endReason() {
        long h = peekHandle();
        if (h == 0) return closedEndReason;
        return RecvEndReason.fromWireOrdinal(nEndReason(h));
    }

    /**
     * Return {@code true} while the receiver owns a live transport.
     *
     * @return liveness state of the underlying SRT socket
     */
    public boolean isAlive() {
        if (peekHandle() == 0) return false;
        return nIsAlive(peekHandle());
    }

    @Override protected void nativeClose(long h) {
        // Runs at most once per receiver: NativeHandle.close() only calls this for
        // the caller that atomically claimed a non-zero handle. `-1` (nothing was
        // recorded) maps to null, which is what the field already held.
        closedEndReason = RecvEndReason.fromWireOrdinal(nClose(h));
    }

    // --- Natives ---

    private static native long nFromUrl(String url,
        boolean maxAttemptsPresent, int maxAttempts,
        int backoffKind, long backoffBaseMs, long backoffMaxMs,
        int gapBufferCapacity, int overflowPolicy, int mode) throws SrtException;
    private static native long nFromUrlWithConfig(String url,
        boolean maxAttemptsPresent, int maxAttempts,
        int backoffKind, long backoffBaseMs, long backoffMaxMs,
        int gapBufferCapacity, int overflowPolicy, int mode,
        int strict, long pesCapPerPid, long pesCapTotal, boolean cfi,
        int av1, long auCellCap, boolean lenientPsi, long syncBufCap,
        boolean unwrapTimestamps) throws SrtException;
    private static native DemuxEvent nNext(long handle) throws SrtException, DemuxException;
    private static native long nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native SocketStats nSrtStats(long handle);
    private static native long nReconnectAttempts(long handle);
    private static native long nLastSeenMicros(long handle, int pid);
    private static native int nEndReason(long handle);
    private static native int nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
