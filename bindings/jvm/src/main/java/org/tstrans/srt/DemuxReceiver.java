package org.tstrans.srt;

import java.util.Iterator;
import java.util.NoSuchElementException;
import java.util.function.Consumer;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * Single-call convenience wrapper that owns a {@code Demuxer} + an
 * {@code SrtTransport}. Construct with a listener-mode SRT URL
 * ({@code srt://[host]:port?mode=listener&...}); iterate over the emitted
 * {@link DemuxEvent} instances (the same sealed hierarchy
 * {@code org.tstrans.mpegts.Demuxer} produces).
 *
 * <p>Mirrors {@code tstrans.srt.DemuxReceiver}. Wraps
 * {@code tst_pipeline::DemuxReceiver<SrtTransport>}.
 *
 * <pre>{@code
 * DemuxerConfig cfg = DemuxerConfig.builder().build();
 * try (DemuxReceiver rx = DemuxReceiver.fromUrl("srt://:7000?mode=listener")) {
 *     for (DemuxEvent e : rx) {
 *         if (e instanceof DemuxEvent.Video v) { ... }
 *     }
 * }
 * }</pre>
 *
 * <p><b>Thread safety:</b> a single {@code DemuxReceiver} is intended to be
 * iterated from one thread; the sanctioned cross-thread wake is
 * {@link #cancelHandle()}'s {@code cancel()}, which wakes a thread parked in
 * iteration. {@link #addByteSink} is a single-iterator op — register sinks before
 * iterating, or between {@code next()} calls.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * {@code close()} is memory-safe against ANY concurrent native call: it claims the
 * registry id atomically and the leased {@code HandleRegistry} guarantees no
 * use-after-free/double-free — a racing call either runs or throws a clean
 * {@link IllegalStateException}. After close, further calls throw
 * {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> sample payloads and the byte-sink
 * callbacks deliver heap {@code byte[]} copies; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 */
public final class DemuxReceiver extends NativeHandle implements Iterable<DemuxEvent> {
    static { NativeLoader.load(); }

    /** Package-private constructor from a native handle. */
    DemuxReceiver(long h) { setHandle(h); }

    /**
     * Bind a listener-mode SRT receiver on {@code url}, accept the first
     * incoming connection, and demux the stream with default options.
     *
     * <p>The URL must use {@code mode=listener}. An empty host
     * ({@code srt://:7000?mode=listener}) binds to {@code 0.0.0.0}.
     *
     * @param url {@code srt://[host]:port?mode=listener[&key=value&...]}
     * @return a connected {@code DemuxReceiver}
     * @throws SrtException {@code CONFIG_INVALID} if the URL is malformed or uses
     *     a non-listener mode; {@code TIMEOUT} on accept timeout;
     *     {@code CONNECT_FAILED} if the socket cannot be bound;
     *     {@code ACCEPT_FAILED} on handshake rejection
     */
    public static DemuxReceiver fromUrl(String url) throws SrtException {
        long h = nFromUrl(url);
        if (h == 0) {
            // nFromUrl throws a pending SrtException; JNI re-raises. Unreachable
            // in practice, but satisfies the compiler.
            throw new SrtException(SrtException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    /**
     * Same as {@link #fromUrl(String)} but with an explicit
     * {@link DemuxerConfig}.
     *
     * @param url         {@code srt://[host]:port?mode=listener[&...]}
     * @param demuxConfig the demuxer configuration
     * @return a connected {@code DemuxReceiver}
     * @throws SrtException as in {@link #fromUrl(String)}
     */
    public static DemuxReceiver fromUrl(String url, DemuxerConfig demuxConfig) throws SrtException {
        long h = nFromUrlWithConfig(
            url,
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
        if (h == 0) {
            throw new SrtException(SrtException.Kind.IO,
                "nFromUrlWithConfig returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    /**
     * Iterate the demuxed events. Each {@code hasNext()} blocks on the next
     * {@code recv_event} until an event arrives, the transport closes cleanly
     * (clean EOF → iteration ends), or an error occurs. Checked
     * {@link SrtException} / {@link DemuxException} surfaced by the native pull
     * are wrapped in an unchecked {@link RuntimeException} (the {@code Iterator}
     * contract forbids checked exceptions); a byte-sink exception (see
     * {@link #addByteSink}) is re-raised fail-loud and stops iteration.
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
     * Register a fan-out callback that receives every 188-byte TS packet — as a
     * fresh {@code byte[]} — BEFORE the demuxer parses it. Sinks fire in
     * registration order; registration is append-only for the receiver's
     * lifetime (there is no removal). Useful for tee'ing the raw transport
     * stream (record-to-disk, parallel parser, etc.) without consuming the event
     * iterator.
     *
     * <p><b>Fail-loud:</b> if {@code callback} throws, the exception is captured
     * (first error wins; later per-packet errors are dropped) and re-raised from
     * the <em>next</em> iteration step, which then stops iteration.
     *
     * <p><b>Registration timing:</b> register sinks BEFORE iterating, or between
     * {@code next()} calls. This method is NOT safe to call concurrently with an
     * in-flight {@code next()} (single-threaded contract); the only sanctioned
     * cross-thread stop is {@link #cancelHandle()}{@code .cancel()}.
     *
     * <p><b>Re-entrancy:</b> the callback runs on the receiver's own thread
     * inside the recv loop. It MUST NOT re-enter this receiver (no
     * {@code next()}/{@code close()}/{@code stats()} from inside the sink). Keep
     * the callback cheap — a slow sink throttles the receiver.
     *
     * @param callback invoked with a fresh {@code byte[]} per 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     */
    public void addByteSink(Consumer<byte[]> callback) {
        ensureOpen("DemuxReceiver is closed");
        nAddByteSink(peekHandle(), callback);
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * wakes a thread parked in iteration; that pull then ends or throws.
     *
     * @return a new {@link CancelHandle}
     * @throws IllegalStateException if the receiver is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("DemuxReceiver is closed");
        long ch = nCancelHandle(peekHandle());
        return new CancelHandle(ch);
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot.
     *
     * <p>May block briefly if another thread is parked in {@code next()} — the
     * snapshot is taken under the receiver's resource lock, which a parked recv
     * holds until it returns.
     *
     * @return the wire-stats snapshot (never null in normal operation)
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("DemuxReceiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * Combined {@code (SocketStats, MuxerStats)} snapshot. The socket stats carry
     * the pipeline-tracked byte/packet counters; the muxer stats reshape the
     * demux-side counters (packets/bytes received, program maps seen) so callers
     * read the same {@link TransportStats} shape on both {@code MuxSender} and
     * {@code DemuxReceiver}.
     *
     * <p>May block briefly if another thread is parked in {@code next()} — the
     * snapshot is taken under the receiver's resource lock, which a parked recv
     * holds until it returns.
     *
     * @return the combined stats snapshot
     * @throws IllegalStateException if the receiver is closed
     */
    public TransportStats stats() {
        ensureOpen("DemuxReceiver is closed");
        return nStats(peekHandle());
    }

    /**
     * Wall-clock time the stream identified by {@code pid} last carried a
     * demuxed item through this receiver (last emitted event), as a
     * Unix-epoch microsecond count. {@code null} if {@code pid} was never
     * seen — including an unrecognized PID (plain {@code int}-to-{@code
     * u16} cast, no range check, same as the {@code pmtPid}/{@code pcrPid}
     * parameters elsewhere in this binding) or before any event has arrived.
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
        ensureOpen("DemuxReceiver is closed");
        long v = nLastSeenMicros(peekHandle(), pid);
        return v < 0 ? null : v;
    }

    /**
     * Close the receiver. Closes the underlying libsrt socket. Idempotent —
     * subsequent calls are no-ops.
     *
     * <p>If a thread is parked in iteration ({@code next()}), {@code close()}
     * cancels first: it fires the receiver's cancel target before taking the
     * resource lock the parked recv holds, so that iteration ends promptly with
     * {@code SrtException(BROKEN)} — the cancel closes the libsrt socket under
     * it; the managed {@link ManagedDemuxReceiver} surfaces the same close as
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

    // --- Natives ---

    private static native long nFromUrl(String url) throws SrtException;
    private static native long nFromUrlWithConfig(String url, int strict, long pesCapPerPid,
        long pesCapTotal, boolean cfi, int av1, long auCellCap, boolean lenientPsi,
        long syncBufCap, boolean unwrapTimestamps) throws SrtException;
    private static native DemuxEvent nNext(long handle) throws SrtException, DemuxException;
    private static native void nAddByteSink(long handle, Consumer<byte[]> callback);
    private static native long nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native TransportStats nStats(long handle);
    private static native long nLastSeenMicros(long handle, int pid);
    private static native void nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
