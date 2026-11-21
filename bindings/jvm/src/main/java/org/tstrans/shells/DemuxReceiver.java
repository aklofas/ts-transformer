package org.tstrans.shells;

import java.time.Instant;
import java.time.temporal.ChronoUnit;
import java.util.Iterator;
import java.util.NoSuchElementException;
import java.net.InetSocketAddress;
import java.util.Optional;
import java.util.OptionalInt;
import java.util.concurrent.atomic.AtomicLong;
import org.tstrans.BindingException;
import org.tstrans.DemuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.internal.EndReasonSnapshot;
import org.tstrans.internal.NativeAddr;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.rtp.RtpStats;

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
 * <p><b>The start and the end of a stream:</b> the receiver demuxes its first packet
 * once it has found four sync bytes ({@code 0x47}) 188 bytes apart, and again after
 * it lost the alignment. When the transport ends (the peer's end of stream, a broken
 * transport or a managed receiver's give-up) the one to three packets still waiting
 * for that confirmation are demuxed first and their events come before the end, when
 * every packet boundary present reads {@code 0x47}, the first byte of a trailing
 * partial packet included (that partial packet's own bytes are dropped). So a stream
 * of one, two or three packets yields its events. One boundary that fails refuses the
 * whole tail, a single trailing byte that is not {@code 0x47} included, and a single
 * packet right behind damage is lost when a {@code 0x47} lies in the 187 bytes before
 * it. This is weaker evidence than four sync bytes: bytes that are not TS can reach
 * the demuxer as a packet at an end. A cancel, or {@link #close()} from another
 * thread, ends the stream too: the call it interrupts may still return the first
 * event they yield, and every later call throws {@code CLOSED} (after
 * {@code close()}, {@code IllegalStateException}), so the rest are lost. A
 * {@code BACKPRESSURE} tick is not an end.
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
    private final RtpCounters rtp;

    /** Wrap a native handle returned by {@link #openHandle(String)}. */
    protected DemuxReceiver(long handle) {
        setHandle(handle);
        rtp = new RtpCounters(nRtpStats(handle));
    }

    /**
     * Open a receiver on the transport named by {@code url}'s scheme and demux
     * the stream with default options. An {@code srt://} URL must carry
     * {@code mode=listener}: the open accepts ONE peer and blocks until one
     * connects. No {@link #cancelHandle()} exists until the open returns; to end
     * that first accept from another thread, pass an {@link OpenCancel} to
     * {@link #open(String, DemuxerConfig, OpenCancel)} and cancel it.
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
     * Same as {@link #open(String, DemuxerConfig)}, under {@code cancel}: its
     * {@link OpenCancel#cancel()} from another thread ends an {@code srt://}
     * first accept, and an object already cancelled refuses the open before the
     * URL is parsed. Either way the open throws the URL's transport exception
     * with kind {@code CLOSED} (see {@link OpenCancel} for every rule).
     *
     * @param url         {@code scheme://...} with per-transport query options
     * @param demuxConfig the demuxer configuration; not {@code null}
     *                    ({@code DemuxerConfig.builder().build()} gives the defaults)
     * @param cancel      the open's cancel; {@code null} is the same as
     *                    {@link #open(String, DemuxerConfig)}
     * @return an open {@code DemuxReceiver}
     * @throws NullPointerException if {@code demuxConfig} is {@code null}
     * @throws IllegalArgumentException if the scheme is not supported
     * @throws IllegalStateException if {@code cancel} is closed
     * @throws BindingException as in {@link #open(String)}; kind {@code CLOSED}
     *     when {@code cancel} was cancelled by the time the open finished
     */
    public static DemuxReceiver open(String url, DemuxerConfig demuxConfig, OpenCancel cancel)
            throws BindingException {
        return new DemuxReceiver(openWithConfig(url, demuxConfig, cancel));
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
        return nOpen(url, null);
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
        return openWithConfig(url, demuxConfig, null);
    }

    private static long openWithConfig(String url, DemuxerConfig demuxConfig, OpenCancel cancel)
            throws BindingException {
        return nOpenWithConfig(
            url,
            demuxConfig.strictMode().code(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().code(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps(), cancel);
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
     * <p>At the end: once this has returned {@code null}, every later call returns
     * {@code null} again, unless a cancel fires afterwards (then {@code CLOSED}). A
     * transport failure other than a cancel is thrown once, after the events the
     * end released, then {@code null} follows, unless a cancel fires afterwards
     * (then {@code CLOSED}); after a cancel every call throws {@code CLOSED}.
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
     * <p>What the fields mean per scheme (every other field reads 0 on these):
     * <table>
     * <caption>socketStats() fields by scheme</caption>
     * <tr><th>scheme</th><th>bytes / packets</th><th>packetsDropped*</th><th>other</th></tr>
     * <tr><td>{@code udp://}</td><td>bytes; a packet is one datagram</td>
     *     <td>socket send / receive errors</td><td></td></tr>
     * <tr><td>{@code tcp://}</td><td>bytes; a packet is one {@code send} / {@code recv} call</td>
     *     <td>send / receive errors</td><td></td></tr>
     * <tr><td>{@code rist://}</td><td>bytes and packets</td>
     *     <td>{@code packetsDroppedRecv}: librist's lost-packet count plus oversize or malformed
     *     blocks the receiver dropped</td>
     *     <td>{@code packetsRetransmitted} (a sender's count; a receiver reads 0),
     *     {@code linkBandwidthBps} (librist's estimate),
     *     {@code rttUs}</td></tr>
     * </table>
     * On a receiver the send-side counters read 0, and on a sender the receive-side ones do;
     * {@code packetsDroppedSend} is always 0 on {@code rist://}.
     *
     * <p>On {@code rist://} librist reports its part once a second, so the lost count,
     * {@code packetsRetransmitted}, {@code linkBandwidthBps} and {@code rttUs} lag, and
     * {@code linkBandwidthBps} can read 0 for a long time (it stayed 0 on a loopback link), and
     * librist measures the round trip in whole milliseconds, so {@code rttUs} reads 0 for a round
     * trip under 1&nbsp;ms; neither 0 means there is no link. The byte and packet counts the
     * wrapper keeps itself are not delayed, but a receiver's {@code bytesReceived} and
     * {@code packetsReceived} count what the transport has read, a whole block at a time (a
     * datagram or a read on {@code udp://} and {@code tcp://}, a block, 1316 bytes from this
     * library's senders, on {@code rist://}), so they can run ahead of what the receiving calls
     * have returned so far. The call waits behind a receive or send parked on another thread.
     *
     * @return the wire-stats snapshot (never null in normal operation)
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("DemuxReceiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * The receive counters: TS bytes and packets the receiver read, program maps and
     * PMT versions seen, continuity gaps and nonconformant items the demuxer met. The
     * wire's own counters are {@link #socketStats()}.
     *
     * <p>Waits while another thread is parked in {@code next()}: the counters live in
     * the receiver, which a parked receive holds until it returns.
     *
     * @return the counters as they stand now
     * @throws IllegalStateException if the receiver is closed
     */
    public DemuxReceiverStats stats() {
        ensureOpen("DemuxReceiver is closed");
        return nStats(peekHandle());
    }

    /**
     * The RTP counters of a receiver over {@code rtp://}, an RTSP session or a publish
     * mount: datagrams dropped as malformed, and those the URL's {@code source} key
     * turned away. Empty on every other transport. It never waits behind a
     * {@code next()} parked on another thread, a read never returns less than an
     * earlier one, and after {@link #close()} it returns the counters as the close
     * left them.
     *
     * @return the RTP counters, or empty off RTP
     */
    public Optional<RtpStats> rtpStats() {
        long h = peekHandle();
        return rtp.read(h == 0 ? null : nRtpStats(h));
    }

    /**
     * Wall-clock time the stream identified by {@code pid} last carried a
     * demuxed item through this receiver (last emitted event), to the
     * microsecond. Empty if {@code pid} was never seen (including an
     * unrecognized PID) or before any event has arrived.
     *
     * <p><b>Blocking:</b> this takes the same internal resource lock a
     * parked {@code next()} call holds — if an iteration is currently in
     * flight on this receiver, {@code lastSeen} blocks until it
     * returns (an event, end of stream, or an error), which on a
     * fully-quiet stream may be indefinite. Call between {@code next()}
     * calls, or from the same thread driving the iteration, to avoid
     * blocking.
     *
     * @param pid the stream PID to query, in 0..=0x1FFF
     * @return the instant, or empty
     * @throws IllegalArgumentException if {@code pid} is outside 0..=0x1FFF
     * @throws IllegalStateException if the receiver is closed
     */
    public Optional<Instant> lastSeen(int pid) {
        long v = lastSeenRaw(pid);
        return v < 0 ? Optional.empty() : Optional.of(Instant.EPOCH.plus(v, ChronoUnit.MICROS));
    }

    /**
     * {@link #lastSeen(int)} as a Unix-epoch microsecond count, {@code null}
     * where {@code lastSeen} is empty; same blocking and errors.
     *
     * @param pid the stream PID to query, in 0..=0x1FFF
     * @return the last-seen timestamp in Unix-epoch microseconds, or
     *     {@code null}
     * @throws IllegalArgumentException if {@code pid} is outside 0..=0x1FFF
     * @throws IllegalStateException if the receiver is closed
     * @deprecated use {@link #lastSeen(int)}; removed in 0.12.0
     */
    @Deprecated(since = "0.11.0", forRemoval = true)
    public Long lastSeenMicros(int pid) {
        long v = lastSeenRaw(pid);
        return v < 0 ? null : v;
    }

    /** The native microsecond count for {@code pid}, negative when never seen. */
    private long lastSeenRaw(int pid) {
        if (pid < 0 || pid > 0x1FFF) {
            throw new IllegalArgumentException(
                "pid must be in 0..=0x1FFF, got 0x" + Integer.toHexString(pid));
        }
        ensureOpen("DemuxReceiver is closed");
        return nLastSeenMicros(peekHandle(), pid);
    }

    /**
     * Why the receive session ended; empty while it has not ended.
     * Every plain receive transport ({@code rtp}, {@code srt}, {@code udp},
     * {@code tcp}, {@code rist}) records one; {@code srt} never records a
     * clean teardown, because libsrt reports a peer's close as a broken link.
     * Lock-free, so it answers while
     * iteration is parked on another thread; once the receiver is closed it
     * returns the reason as it stood after close.
     *
     * @return the end reason, or empty
     */
    public Optional<EndReason> endReason() {
        long h = peekHandle();
        if (h == 0) return Optional.ofNullable(closedEndReason);
        return Optional.ofNullable(EndReason.fromWireValue(nEndReason(h)));
    }

    /**
     * Free-text detail for {@link #endReason()}: the message carried by
     * {@code KEEPALIVE_FAILED}, {@code TRANSPORT_FAILED} and
     * {@code PROTOCOL_ERROR}; empty otherwise.
     *
     * @return the detail, or empty
     */
    public Optional<String> endDetail() {
        long h = peekHandle();
        if (h == 0) return Optional.ofNullable(closedEndDetail);
        return Optional.ofNullable(nEndDetail(h));
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
     * The local port the transport bound when this receiver was opened, for a
     * {@code udp://} or {@code rtp://} URL or a {@code rist://@host:port} listener, so
     * a URL with port 0 reports the port the kernel picked. For an {@code srt://}
     * listener it is the port the accepted connection arrived on: the open returns
     * only once a caller connected, so port 0 in an {@code srt://} URL cannot tell a
     * caller where to dial. Empty for every other shell (a caller, a connection
     * accepted by a listener object, a receiver taken from an RTSP session or a
     * publish mount). The value is the one captured at open; it never waits behind a
     * {@code next()} parked on another thread. A RIST receiver on port 0 needs the Main profile (the
     * default); the Simple profile is refused at open with kind
     * {@code INVALID_CONFIG}, because its second port cannot follow a
     * kernel-picked one.
     *
     * @return the bound port, or empty when the transport reports none
     * @throws IllegalStateException if the receiver is closed
     */
    public OptionalInt localPort() {
        ensureOpen("DemuxReceiver is closed");
        int p = nLocalPort(peekHandle());
        return p < 0 ? OptionalInt.empty() : OptionalInt.of(p);
    }

    /**
     * The address the transport bound when this receiver was opened: present exactly
     * where {@link #localPort()} is, with the same port. The value is the one captured
     * at open; it never waits behind a {@code next()} parked on another thread.
     *
     * @return the bound address, or empty when the transport reports none
     * @throws IllegalStateException if the receiver is closed
     */
    public Optional<InetSocketAddress> localAddr() {
        ensureOpen("DemuxReceiver is closed");
        return Optional.ofNullable(NativeAddr.decode(nLocalAddr(peekHandle())));
    }

    /**
     * The address of the sender of the datagram ({@code udp://}) or block
     * ({@code rist://}) the transport delivered last. Empty until the first one is
     * delivered, and always empty on every other transport ({@code srt://},
     * {@code tcp://}, {@code tcps://}, {@code rtp://} among them), which keeps no such
     * record.
     *
     * <p>It is the source address the datagrams carry, and nothing proves the sender can
     * receive there: a UDP or RIST source address can be forged. Without a
     * {@code source} key in the URL every sender that reaches the receiver is
     * delivered, interleaved, and this names whichever delivered last. An IPv4 sender
     * on a {@code [::]} bind is reported as an {@link java.net.Inet4Address}. The
     * address is built from its bytes, with no name lookup. It never waits behind a
     * {@code next()} parked on another thread.
     *
     * @return the sender's address, or empty
     * @throws IllegalStateException if the receiver is closed
     */
    public Optional<InetSocketAddress> peerAddr() {
        ensureOpen("DemuxReceiver is closed");
        return Optional.ofNullable(NativeAddr.decode(nPeerAddr(peekHandle())));
    }

    /**
     * How much the {@code source} key of a {@code udp://} or {@code rist://} receiver
     * URL turned away: datagrams on {@code udp://}; on {@code rist://}, datagrams from a
     * sender the pin refuses, datagrams whose source address could not be read, and
     * delivered blocks refused on the read path.
     * {@code source=<ip>} admits that IP only; {@code source=first} admits the IP of the
     * first datagram or block delivered and holds it until the receiver is closed. Either
     * compares the IP address, never the port. What is turned away is counted here, and
     * neither sender is told; on {@code udp://} it is in no received counter of
     * {@link #socketStats()}. If the publisher's IP changes
     * under a pin, its datagrams are counted here and the receiver delivers nothing until
     * it is closed and reopened.
     *
     * <p>0 on a receiver without the key, and on every other transport: an
     * {@code rtp://} receiver's own {@code source} key is not counted here. It never
     * waits behind a {@code next()} parked on another thread.
     *
     * @return the count of datagrams or blocks turned away
     * @throws IllegalStateException if the receiver is closed
     */
    public long sourceRejected() {
        ensureOpen("DemuxReceiver is closed");
        return nSourceRejected(peekHandle());
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
        long[] rtpOut = {-1, -1};
        EndReasonSnapshot snapshot = nClose(h, tsBytesOut, rtpOut);
        tsBytesSeen.accumulateAndGet(tsBytesOut[0], Math::max);
        rtp.atClose(rtpOut);
        if (snapshot != null) {
            closedEndDetail = snapshot.detail();
            closedEndReason = snapshot.reason();
        }
    }

    // --- Natives ---

    private static native long nOpen(String url, OpenCancel cancel) throws BindingException;
    private static native long nOpenWithConfig(String url, int strict, long pesCapPerPid,
        long pesCapTotal, boolean cfi, int av1, long auCellCap, boolean lenientPsi,
        long syncBufCap, boolean unwrapTimestamps, OpenCancel cancel) throws BindingException;
    private static native DemuxEvent nNext(long handle) throws BindingException;
    private static native void nGateCached(long handle) throws BindingException;
    private static native long nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native DemuxReceiverStats nStats(long handle);
    private static native RtpStats nRtpStats(long handle);
    private static native long nLastSeenMicros(long handle, int pid);
    private static native int nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native EndReasonSnapshot nClose(long handle, long[] tsBytesOut, long[] rtpOut);
    private static native long nTsBytesDemuxed(long handle);
    private static native int nLocalPort(long handle);
    private static native byte[] nLocalAddr(long handle);
    private static native byte[] nPeerAddr(long handle);
    private static native long nSourceRejected(long handle);
    private static native boolean nIsAlive(long handle);
}
