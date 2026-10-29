package org.tstrans.shells;

import java.util.Iterator;
import java.util.NoSuchElementException;
import java.util.function.Consumer;
import org.tstrans.BindingException;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * Any transport in, {@link DemuxEvent}s out: owns a {@code Demuxer} and a
 * transport. Opened from any transport's URL ({@code srt://}, {@code rtp://},
 * {@code udp://}, {@code tcp://}, {@code tcps://}, {@code rist://}); iterate over the
 * emitted {@link DemuxEvent} instances (the same sealed hierarchy
 * {@code org.tstrans.mpegts.Demuxer} produces). A transport failure throws that
 * transport's exception ({@code SrtException}, {@code RtpException},
 * {@code UdpException}, {@code TcpException}, {@code RistException}), each a
 * {@link BindingException}; a demux failure is a {@link DemuxException}.
 *
 * <p>Mirrors {@code tstrans.shells.DemuxReceiver} in tst-py. Wraps
 * {@code tst_pipeline::DemuxReceiver<Box<dyn RecvTransport>>}.
 *
 * <pre>{@code
 * try (DemuxReceiver rx = DemuxReceiver.open("udp://0.0.0.0:5000")) {
 *     for (DemuxEvent e : rx) {
 *         if (e instanceof DemuxEvent.Video v) { ... }
 *     }
 * }
 * }</pre>
 *
 * <p><b>Thread safety:</b> a single {@code DemuxReceiver} is intended to be
 * iterated from one thread; the sanctioned cross-thread wake is
 * {@link #cancelHandle()}'s {@code cancel()}, which wakes a thread parked in
 * iteration. {@link #addByteSink} is a single-iterator op: register sinks before
 * iterating, or between {@code next()} calls.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * {@code close()} is memory-safe against ANY concurrent native call: it claims the
 * registry id atomically and the leased registry guarantees no
 * use-after-free/double-free; a racing call either runs or throws a clean
 * {@link IllegalStateException}. After close, further calls throw
 * {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> sample payloads and the byte-sink
 * callbacks deliver heap {@code byte[]} copies; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 */
public class DemuxReceiver extends NativeHandle implements Iterable<DemuxEvent> {
    static { NativeLoader.load(); }

    // The end reason as it stood when close() ran; the native handle is gone by
    // then. Written once by nativeClose, read by endReason()/endDetail().
    private volatile EndReason closedEndReason;
    private volatile String closedEndDetail;
    // Set by a close() from this receiver's own byte sink, which only
    // cancels; the pull that then reports end of stream completes the close.
    private volatile boolean closeDeferred;

    /** Wrap a native handle returned by {@link #openHandle(String)}. */
    protected DemuxReceiver(long handle) { setHandle(handle); }

    /**
     * Open a receiver on the transport named by {@code url}'s scheme and demux
     * the stream with default options. An {@code srt://} URL must carry
     * {@code mode=listener}: the open accepts ONE peer, and that first accept is
     * not cancellable because no handle exists until the open returns.
     *
     * @param url {@code scheme://...} with per-transport query options
     * @return an open {@code DemuxReceiver}
     * @throws IllegalArgumentException if the scheme is not one of the six above
     * @throws BindingException the transport's exception for a malformed URL or a
     *     bind/connect fault. An {@code srt://} bind or accept fault is
     *     {@code SrtException(BROKEN)} with the message prefixed {@code bind: }
     *     or {@code accept: }.
     */
    public static DemuxReceiver open(String url) throws BindingException {
        return new DemuxReceiver(openHandle(url));
    }

    /**
     * Same as {@link #open(String)} with explicit demux options.
     *
     * @param url         {@code scheme://...} with per-transport query options
     * @param demuxConfig the demuxer configuration
     * @return an open {@code DemuxReceiver}
     * @throws IllegalArgumentException if the scheme is not supported
     * @throws BindingException as in {@link #open(String)}
     */
    public static DemuxReceiver open(String url, DemuxerConfig demuxConfig)
            throws BindingException {
        return new DemuxReceiver(openHandle(url, demuxConfig));
    }

    /**
     * Open the native receiver (default demux options) and return its handle,
     * for a subclass's own factory. Same contract as {@link #open(String)}.
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
     * Open the native receiver with explicit demux options and return its
     * handle, for a subclass's own factory.
     *
     * @param url         {@code scheme://...} with per-transport query options
     * @param demuxConfig the demuxer configuration
     * @return the native handle, to pass to the constructor
     * @throws IllegalArgumentException if the scheme is not supported
     * @throws BindingException the transport's exception
     */
    protected static long openHandle(String url, DemuxerConfig demuxConfig)
            throws BindingException {
        return nOpenWithConfig(
            url,
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
    }

    /**
     * Receive the next demuxed event. Blocks until an event is produced, the
     * transport ends cleanly, or an error occurs.
     *
     * <p>Unlike {@link #iterator()}, which wraps the checked exceptions in an
     * unchecked {@link RuntimeException} (the {@code Iterator} contract forbids
     * checked ones), this method surfaces them directly. A configured persistent
     * receive deadline (the {@code ?recv_timeout=<ms>} URL key) expiring throws
     * the transport's exception with kind {@code BACKPRESSURE} here; the receiver
     * stays usable and the next {@code recvEvent()} resumes normally.
     *
     * <p>{@code recvEvent()} and {@link #iterator()} share the single-iterator
     * contract documented on the class: do not call this method concurrently
     * with an in-flight iteration on the same receiver.
     *
     * @return the next {@link DemuxEvent}, or {@code null} at end of stream
     * @throws IllegalStateException if the receiver is closed
     * @throws BindingException the transport's exception: {@code CLOSED} if a
     *     cancel fired, {@code BACKPRESSURE} if a persistent deadline expires,
     *     {@code IO} or the transport's own kind otherwise
     * @throws DemuxException on a demux-side error
     */
    public DemuxEvent recvEvent() throws BindingException, DemuxException {
        ensureOpen("DemuxReceiver is closed");
        return pull();
    }

    private DemuxEvent pull() throws BindingException {
        DemuxEvent e = nNext(peekHandle());
        if (e == null && closeDeferred) close();
        return e;
    }

    /**
     * Iterate the demuxed events. Each {@code hasNext()} blocks on the next
     * {@code recv_event} until an event arrives, the transport ends cleanly
     * (the peer closed: iteration ends), or an error occurs. Checked
     * {@link BindingException}s (the transport's exception, {@link DemuxException})
     * surfaced by the native pull are wrapped in an unchecked {@link RuntimeException} (the {@code Iterator}
     * contract forbids checked exceptions); a byte-sink exception (see
     * {@link #addByteSink}) is rethrown by the step after the callback threw,
     * and the receiver cannot be resumed.
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
                    peeked = pull();
                } catch (BindingException e) {
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
     * <p><b>Fail-loud:</b> if the callback throws, the receiver is cancelled, the
     * callback is not called again (sinks registered after one that deferred a
     * close skip the packet on which the deferral happened), and the next iteration step rethrows that
     * exception; the receiver cannot be resumed.
     *
     * <p><b>Registration timing:</b> register sinks BEFORE iterating, or between
     * {@code next()} calls. This method is NOT safe to call concurrently with an
     * in-flight {@code next()} (single-threaded contract); the only sanctioned
     * cross-thread stop is {@link #cancelHandle()}{@code .cancel()}.
     *
     * <p><b>Re-entrancy:</b> the callback runs on the receiver's own thread
     * inside the recv loop. The callback must not call this receiver's methods
     * except {@code close()}, {@code cancelHandle()}, {@code isAlive()} and
     * {@code endReason()}; any other call throws {@code IllegalStateException}.
     * The same holds when the call reaches this receiver through another
     * receiver the callback drives. {@code close()} from the callback cancels;
     * the in-flight iteration step still returns the event it produced, if any,
     * and the iteration then returns end of stream (at once when the step
     * produced none). Keep the callback cheap — a slow sink throttles the
     * receiver.
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
        return new CancelHandle(nCancelHandle(peekHandle()));
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
     * Why the receive session ended, or {@code null} while it has not ended.
     * Every plain receive transport ({@code rtp}, {@code srt}, {@code udp},
     * {@code tcp}, {@code rist}) records one; {@code srt} never records a
     * clean teardown, because libsrt reports a peer's close as a broken link.
     * Lock-free, so it answers while
     * iteration is parked on another thread; once the receiver is closed it
     * returns the reason as it stood after close.
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
     * Close the receiver. Idempotent — subsequent calls are no-ops.
     *
     * <p>If a thread is parked in iteration ({@code next()}), {@code close()}
     * cancels first: it fires the receiver's cancel target before taking the
     * slot the parked recv holds, so that iteration ends promptly with the
     * transport's {@code CLOSED} and {@code close()} returns without waiting for
     * data. Same contract as the Python and C twins.
     *
     * <p>Called from this receiver's own byte sink (see {@link #addByteSink}),
     * whose iteration step holds the receiver, {@code close()} only cancels; the
     * iteration then ends and completes the close. A {@code close()} from
     * anywhere else cancels, waits for an in-flight iteration step to return,
     * and then closes; it wins over such a deferred close.
     */
    @Override public void close() {
        long h = peekHandle();
        // The deferral claims nothing: the handle stays set for the pull
        // that completes the close through super.close().
        if (h != 0 && nDeferClose(h)) {
            closeDeferred = true;
            return;
        }
        // Cleared on both sides so this completed close wins over a deferral.
        // super.close() zeroes the handle before it cancels, so a sink's
        // close() normally sees 0 and does nothing; the second clear covers a
        // sink that read the handle just before that claim.
        closeDeferred = false;
        super.close();
        closeDeferred = false;
    }

    /**
     * Return {@code true} while the receiver owns a live transport.
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

    // --- Natives ---

    private static native long nOpen(String url) throws BindingException;
    private static native long nOpenWithConfig(String url, int strict, long pesCapPerPid,
        long pesCapTotal, boolean cfi, int av1, long auCellCap, boolean lenientPsi,
        long syncBufCap, boolean unwrapTimestamps) throws BindingException;
    private static native DemuxEvent nNext(long handle) throws BindingException;
    private static native void nAddByteSink(long handle, Consumer<byte[]> callback);
    private static native long nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native TransportStats nStats(long handle);
    private static native long nLastSeenMicros(long handle, int pid);
    private static native int nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native boolean nDeferClose(long handle);
    private static native EndReason.Snapshot nClose(long handle);
    private static native boolean nIsAlive(long handle);
}
