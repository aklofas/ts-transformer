package org.tstrans.rtp;

import java.util.Iterator;
import java.util.NoSuchElementException;
import java.util.function.Consumer;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * Single-call convenience wrapper that owns a {@code Demuxer} + an RTP
 * {@code RtpRecvTransport}. Construct with an {@code rtp://host:port} URL
 * (unicast or multicast); iterate over the emitted {@link DemuxEvent} instances
 * (the same sealed hierarchy {@code org.tstrans.mpegts.Demuxer} produces).
 *
 * <p>Mirrors {@code tstrans.rtp.DemuxReceiver}. Wraps
 * {@code tst_pipeline::DemuxReceiver<tst_rtp::RtpRecvTransport>}.
 *
 * <p><b>Thread safety:</b> a single {@code DemuxReceiver} is intended to be
 * iterated from one thread. There is no {@code cancelHandle()} on the RTP
 * convenience wrapper (matching tst-py's surface), so the sanctioned cross-thread
 * operation is {@link #close()}: to stop an iteration currently <em>parked</em> in
 * {@code next()} waiting for the next datagram, another thread may call
 * {@code close()} — it cancels the in-flight recv first (waking the parked
 * {@code next()} within ~100&nbsp;ms), then frees the receiver.
 *
 * <p>{@code close()} is memory-safe against ANY concurrent native call: it claims
 * the registry id atomically and the leased {@code HandleRegistry} guarantees no
 * use-after-free/double-free — a racing call either runs or throws a clean
 * {@link IllegalStateException}. Note a <em>fresh</em> {@code next()} entered
 * <em>after</em> {@code close()} fired its cancel hook is not woken by that same
 * hook; it observes the closed handle and throws {@link IllegalStateException}
 * (single-iterator expectation). After {@code close()}, further calls throw
 * {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> sample payloads and the byte-sink
 * callbacks deliver heap {@code byte[]} copies; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only and will be added in a future release.
 *
 * <pre>{@code
 * try (DemuxReceiver rx = DemuxReceiver.fromUrl("rtp://0.0.0.0:5004")) {
 *     for (DemuxEvent e : rx) {
 *         if (e instanceof DemuxEvent.Video v) { ... }
 *     }
 * }
 * }</pre>
 */
public final class DemuxReceiver extends NativeHandle implements Iterable<DemuxEvent> {
    static { NativeLoader.load(); }

    // Populated by nativeClose from nClose's close-time snapshot (see
    // endReason()/endDetail()'s javadoc for why: once closed, peekHandle()
    // is 0 and there is no handle left to pass to a live-getter native).
    private volatile StreamEndReason closedEndReason;
    private volatile String closedEndDetail;

    DemuxReceiver(long h) { setHandle(h); }

    /**
     * Bind a receiver to {@code url} with default demux options.
     *
     * @param url {@code rtp://host:port} (unicast or multicast)
     * @return a bound {@code DemuxReceiver}
     * @throws RtpException {@code TRANSPORT} on URL-parse / socket-bind failure
     */
    public static DemuxReceiver fromUrl(String url) throws RtpException {
        long h = nFromUrl(url);
        if (h == 0) {
            throw new RtpException(RtpException.Kind.TRANSPORT,
                "nFromUrl returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    /**
     * Same as {@link #fromUrl(String)} but with an explicit {@link DemuxerConfig}.
     *
     * @param url         {@code rtp://host:port}
     * @param demuxConfig the demuxer configuration
     * @return a bound {@code DemuxReceiver}
     * @throws RtpException as in {@link #fromUrl(String)}
     */
    public static DemuxReceiver fromUrl(String url, DemuxerConfig demuxConfig) throws RtpException {
        long h = nFromUrlWithConfig(
            url,
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
        if (h == 0) {
            throw new RtpException(RtpException.Kind.TRANSPORT,
                "nFromUrlWithConfig returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    /**
     * Receive the next demuxed event. Blocks until an event is produced, the
     * transport drains cleanly (rare for connectionless RTP/UDP — see
     * {@link #iterator()}), or an error occurs.
     *
     * <p>Unlike {@link #iterator()}, which wraps checked
     * {@link RtpException}/{@link DemuxException} in an unchecked
     * {@link RuntimeException} (the {@code Iterator} contract forbids checked
     * exceptions), this method surfaces them directly as catchable checked
     * exceptions — in particular, a configured persistent recv deadline (the
     * {@code ?recv_timeout=<ms>} URL knob) expiring throws
     * {@code RtpException(TIMEOUT)} here rather than a wrapped
     * {@code RuntimeException}. The receiver stays usable after a
     * {@code TIMEOUT} (retryable) — a subsequent {@code recvEvent()} call
     * resumes normally.
     *
     * <p>{@code recvEvent()} and {@link #iterator()} share the single-iterator
     * contract documented on the class: do not call this method concurrently
     * with an in-flight iteration on the same receiver.
     *
     * @return the next {@link DemuxEvent}, or {@code null} at end of stream
     * @throws IllegalStateException if the receiver is closed
     * @throws RtpException {@code CANCELLED} if a cancel fired; {@code TRANSPORT}
     *     otherwise; {@code TIMEOUT} if the persistent {@code ?recv_timeout=}
     *     deadline expires
     * @throws DemuxException on a demux-side error
     */
    public DemuxEvent recvEvent() throws RtpException, DemuxException {
        ensureOpen("DemuxReceiver is closed");
        return nNext(peekHandle());
    }

    /**
     * Iterate the demuxed events. Each {@code hasNext()} blocks on the next
     * {@code recv_event} until an event arrives, the transport drains cleanly
     * (→ end of iteration; rare for connectionless RTP/UDP), or an error occurs.
     * Checked {@link RtpException} / {@link DemuxException} surfaced by the native
     * pull are wrapped in an unchecked {@link RuntimeException} (the
     * {@code Iterator} contract forbids checked exceptions); a byte-sink exception
     * (see {@link #addByteSink}) is re-raised fail-loud and stops iteration.
     *
     * <p>A cross-thread {@link #close()} (the watchdog pattern below) does NOT end
     * iteration via a clean {@code null}/EOF: it cancels the in-flight recv, which
     * surfaces as an {@link RtpException} of kind {@code CANCELLED} wrapped in a
     * {@code RuntimeException}. Catch that to distinguish a deliberate teardown
     * from a real error.
     *
     * <p>Note: RTP/UDP is connectionless — a remote sender closing does NOT end
     * this iteration; it parks on the next datagram. Break out of the loop on a
     * sentinel event, or call {@link #close()} from another thread.
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
                } catch (RtpException | DemuxException e) {
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
     * registration order; registration is append-only for the receiver's lifetime.
     *
     * <p><b>Fail-loud:</b> if {@code callback} throws, the exception is captured
     * (first error wins; later per-packet errors are dropped) and re-raised from
     * the <em>next</em> iteration step, which then stops iteration.
     *
     * <p><b>Registration timing:</b> register sinks BEFORE iterating, or between
     * {@code next()} calls.
     *
     * <p><b>Re-entrancy:</b> the callback runs on the receiver's own thread inside
     * the recv loop. It MUST NOT re-enter this receiver (no
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
     * Combined {@code (SocketStats, MuxerStats)} snapshot. The socket stats carry
     * the pipeline-tracked recv byte/packet counters; the muxer stats reshape the
     * demux-side counters so callers read the same {@link TransportStats} shape on
     * both {@code MuxSender} and {@code DemuxReceiver}.
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
     * Close the receiver. Cancels any in-flight {@code next()} first (waking a
     * parked iteration), then frees the underlying RTP socket. May be called from
     * another thread to stop an iteration that is currently <em>parked</em> in
     * {@code next()} (the cancel wakes the parked recv, which releases the inner
     * receiver before it is freed). Do NOT race {@code close()} against any other
     * concurrent call on this receiver (a second {@code next()}, {@code stats()},
     * {@code addByteSink}, etc.) — the single-iterator contract still applies.
     * Idempotent.
     */
    @Override
    public void close() { super.close(); }

    /** Whether the receiver owns a live transport. */
    public boolean isAlive() {
        if (peekHandle() == 0) return false;
        return nIsAlive(peekHandle());
    }

    /**
     * Why the receive session ended, or {@code null} if it hasn't ended yet
     * (or ended through a path this arc doesn't instrument). Still readable
     * after {@link #close()} — the close path snapshots the reason before
     * the underlying native resource is freed.
     *
     * <p><b>Blocking:</b> while open, this takes the same internal resource
     * lock a parked {@link #recvEvent()} / iteration step holds — if a recv
     * is currently in flight on this receiver, {@code endReason()} blocks
     * until it returns (an event, a clean drain, or an error). Call after
     * the recv loop observes its terminal outcome, or from the same thread
     * driving the recv loop, to avoid blocking. Once closed, this never
     * blocks (returns the cached snapshot).
     *
     * <p><b>Cross-thread close race:</b> a concurrent call from another
     * thread while {@link #close()} is in flight may briefly observe
     * {@code null} before {@code close()} finishes — {@code peekHandle()}
     * reports closed as soon as {@code close()} claims the handle, before
     * the native teardown (which computes the snapshot) completes.
     */
    public StreamEndReason endReason() {
        long h = peekHandle();
        if (h == 0) return closedEndReason;
        return StreamEndReason.fromWireOrdinal(nEndReason(h));
    }

    /**
     * Free-text detail for {@link #endReason()} — the message carried by
     * {@code KEEPALIVE_FAILED} / {@code TRANSPORT_FAILED} /
     * {@code PROTOCOL_ERROR}; {@code null} for every other reason (including
     * "hasn't ended yet"). Still readable after {@link #close()}. Same
     * blocking / cross-thread-close-race caveats as {@link #endReason()}.
     */
    public String endDetail() {
        long h = peekHandle();
        if (h == 0) return closedEndDetail;
        return nEndDetail(h);
    }

    @Override protected void nativeClose(long h) {
        EndReasonSnapshot snapshot = nClose(h);
        if (snapshot == null) return;
        // Detail FIRST, reason SECOND: a reader that observes the (volatile)
        // reason write is then guaranteed (JMM happens-before) to also see
        // this detail write, which happened-before it in program order.
        closedEndDetail = snapshot.detail;
        closedEndReason = snapshot.reason;
    }

    // --- Natives ---

    private static native long nFromUrl(String url) throws RtpException;
    private static native long nFromUrlWithConfig(String url, int strict, long pesCapPerPid,
        long pesCapTotal, boolean cfi, int av1, long auCellCap, boolean lenientPsi,
        long syncBufCap, boolean unwrapTimestamps) throws RtpException;
    private static native DemuxEvent nNext(long handle) throws RtpException, DemuxException;
    private static native void nAddByteSink(long handle, Consumer<byte[]> callback);
    private static native TransportStats nStats(long handle);
    private static native long nLastSeenMicros(long handle, int pid);
    private static native boolean nIsAlive(long handle);
    private static native int nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native EndReasonSnapshot nClose(long handle);
}
