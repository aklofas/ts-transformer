package org.tstrans.shells;

import java.util.Iterator;
import java.util.NoSuchElementException;
import java.util.concurrent.atomic.AtomicLong;
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
 * iteration. {@code close()} is synchronized; do not hold the receiver's
 * monitor while iterating (a {@code synchronized (rx)} loop would block a
 * cross-thread {@code close()} behind the parked {@code next()}).
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * {@code close()} is memory-safe against ANY concurrent native call: it claims the
 * registry id atomically and the leased registry guarantees no
 * use-after-free/double-free; a racing call either runs or throws a clean
 * {@link IllegalStateException}. After close, further calls throw
 * {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> sample payloads are heap
 * {@code byte[]} copies; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 */
public class DemuxReceiver extends NativeHandle implements Iterable<DemuxEvent> {
    static { NativeLoader.load(); }

    // The end reason as it stood when close() ran; the native handle is gone by
    // then. Written once by nativeClose, read by endReason()/endDetail().
    private volatile EndReason closedEndReason;
    private volatile String closedEndDetail;
    // The largest fed-byte count any read or the close saw, so a read never
    // goes backwards across close().
    private final AtomicLong tsBytesSeen = new AtomicLong();

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
     * <p>After {@code cancelHandle().cancel()} this throws the transport's
     * {@code CLOSED}, buffered data included.
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
        return nNext(peekHandle());
    }

    /**
     * Iterate the demuxed events. Each {@code hasNext()} blocks on the next
     * {@code recv_event} until an event arrives, the transport ends cleanly
     * (the peer closed: iteration ends), or an error occurs. Checked
     * {@link BindingException}s (the transport's exception, {@link DemuxException})
     * surfaced by the native pull are wrapped in an unchecked {@link RuntimeException} (the {@code Iterator}
     * contract forbids checked exceptions).
     *
     * <p>After {@code cancelHandle().cancel()} this throws the transport's
     * {@code CLOSED}, buffered data included: an event {@code hasNext()}
     * already returned {@code true} for is not served by a {@code next()}
     * after the cancel.
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
                boolean cached = peeked != null;
                if (!hasNext()) throw new NoSuchElementException();
                DemuxEvent e = peeked; peeked = null;
                // An event held since hasNext() is buffered output: a cancel
                // that landed in between refuses it, as a fresh pull would.
                if (cached) {
                    try {
                        nGateCached(peekHandle());
                    } catch (BindingException ex) {
                        throw new RuntimeException(ex);
                    }
                }
                return e;
            }
        };
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
     * seen — including an unrecognized PID — or before any event has arrived.
     * {@code pid} must be in 0..=0x1FFF, else {@link IllegalArgumentException}.
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
     * @throws IllegalArgumentException if {@code pid} is outside 0..=0x1FFF
     * @throws IllegalStateException if the receiver is closed
     */
    public Long lastSeenMicros(int pid) {
        if (pid < 0 || pid > 0x1FFF) {
            throw new IllegalArgumentException(
                "pid must be in 0..=0x1FFF, got 0x" + Integer.toHexString(pid));
        }
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
     * Close the receiver. Idempotent: a later call returns at once.
     * Concurrent calls are serialised: a call that arrives while another
     * thread's close is in progress waits for it to finish, so once any
     * {@code close()} has returned, {@link #tsBytesDemuxed()} and
     * {@link #endReason()} read the final values.
     *
     * <p>If a thread is parked in iteration ({@code next()}), {@code close()}
     * cancels first: it fires the receiver's cancel target before taking the
     * slot the parked recv holds, so that iteration ends promptly with the
     * transport's {@code CLOSED} and {@code close()} returns without waiting for
     * data. tst-py's {@code close()} has the same contract. The C ABI's does
     * not: there {@code _close} must not run while another call on the
     * handle is in flight, so a C caller cancels, waits for the parked call
     * to return, then closes.
     */
    @Override public synchronized void close() {
        super.close();
    }

    /**
     * Aligned TS bytes this receiver fed its demuxer since it opened. Never waits on
     * the receive slot, so it answers while another thread is parked in {@code next()};
     * after {@link #close()} it returns the value at close. A read never returns less
     * than an earlier one.
     *
     * @return the byte count, a multiple of 188
     */
    public long tsBytesDemuxed() {
        long h = peekHandle();
        long v = h == 0 ? -1 : nTsBytesDemuxed(h);
        return tsBytesSeen.accumulateAndGet(v, Math::max);
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
        long[] tsBytesOut = {-1};
        EndReason.Snapshot snapshot = nClose(h, tsBytesOut);
        tsBytesSeen.accumulateAndGet(tsBytesOut[0], Math::max);
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
    private static native void nGateCached(long handle) throws BindingException;
    private static native long nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native TransportStats nStats(long handle);
    private static native long nLastSeenMicros(long handle, int pid);
    private static native int nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native EndReason.Snapshot nClose(long handle, long[] tsBytesOut);
    private static native long nTsBytesDemuxed(long handle);
    private static native boolean nIsAlive(long handle);
}
