package org.tstrans.rtp;

import java.net.InetSocketAddress;
import java.util.Iterator;
import java.util.NoSuchElementException;
import java.util.Optional;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;
import org.tstrans.internal.EndReasonSnapshot;
import org.tstrans.internal.NativeAddr;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.EndReason;
import org.tstrans.shells.SocketStats;

/**
 * Blocking H.264-over-RTP receiver. Wraps {@code tst_rtp::H264Receiver}.
 * Mirrors tst-py {@code tstrans.rtp.H264Receiver}.
 *
 * <h3>Constructing</h3>
 *
 * <p>{@link #listen(String)} / {@link #listen(String, H264DepayConfig)} — bind a
 * UDP socket to an {@code rtp://host:port?pt=N} URL and return a ready receiver.
 * The {@code ?pt=} query parameter is required (range 1..=127; value 33 is
 * rejected — use {@link org.tstrans.shells.DemuxReceiver} for MPEG-TS streams).
 *
 * <h3>Receiving</h3>
 *
 * <p>{@link #recvAu()} blocks until a complete H.264 Access Unit is reassembled
 * or EOS. Returns {@link H264AccessUnit} on success, {@code null} at EOS (the
 * RTSP server ended the session in an orderly way; a failed RTSP control
 * connection also ends it with {@code null}, and {@link #endReason()} names it), or
 * throws {@link RtpException} on transport failure. After a cancel it returns the
 * AUs it already holds, then throws {@link RtpException} with kind {@code CLOSED}.
 * A {@link #close()} on another thread cancels first, so a call parked in
 * {@code recvAu()} ends the same way. A call that reaches the native receiver after
 * a {@code close()} began throws {@link IllegalStateException}; one that reached it
 * first but loses it to the {@code close()} before it starts receiving throws
 * {@code CLOSED}. A {@code close()} never makes {@code recvAu()} return {@code null}.
 *
 * <h3>Thread safety</h3>
 *
 * <p>A single {@code H264Receiver} must be iterated from one thread (single-iterator
 * contract). The one sanctioned cross-thread operation is {@link #cancelHandle()}:
 * obtain a {@link CancelHandle} BEFORE the potentially-blocking {@link #recvAu()}
 * call, then call {@link CancelHandle#cancel()} from another thread to wake
 * the parked recv within ~100&nbsp;ms. {@link #close()} also fires the cancel
 * handle before freeing the receiver and is safe to call from another thread to
 * stop an iteration currently parked in {@code recvAu()}.
 *
 * <h3>Stats</h3>
 *
 * <p>Three complementary views, mirroring {@code tst_rtp::H264Receiver}:
 * <ul>
 *   <li>{@link #socketStats()} — wire-level throughput (bytes/packets received), the
 *       same {@link SocketStats} the receiving shells' {@code socketStats()} return.
 *   <li>{@link #rtpStats()} — protocol-level counters (malformed packets, and
 *       datagrams rejected by a {@code ?source=} pin).
 *   <li>{@link #depayStats()} — RFC 6184 depacketizer internals (AU counts, seq
 *       gaps, parameter-set updates).
 * </ul>
 *
 * <h3>Lifecycle</h3>
 *
 * <p>{@link #close()} fires the cancel handle and frees the native receiver; for a
 * receiver made by {@link RtspSession#intoH264Receiver()} it also closes the RTSP
 * client, and {@link #endReason()} then reads {@code CLEAN_TEARDOWN} only when the
 * server accepted the TEARDOWN within the close's bound and no {@link #recvAu()} was
 * in progress, otherwise {@code CANCELLED} (see {@link #close()}). Subsequent
 * {@link #recvAu()} throws {@link IllegalStateException}. Idempotent.
 *
 * <pre>{@code
 * try (H264Receiver rx = H264Receiver.listen("rtp://0.0.0.0:5004?pt=96")) {
 *     H264AccessUnit au;
 *     while ((au = rx.recvAu()) != null) {
 *         byte[] annexb = au.annexb();
 *         // ... decode or re-mux ...
 *     }
 * }
 * }</pre>
 */
public final class H264Receiver extends NativeHandle implements Iterable<H264AccessUnit> {
    static { NativeLoader.load(); }

    // Populated by nativeClose from nClose's close-time snapshot (see
    // endReason()/endDetail()'s javadoc for why: once closed, peekHandle()
    // is 0 and there is no handle left to pass to a live-getter native).
    private volatile EndReason closedEndReason;
    private volatile String closedEndDetail;

    H264Receiver(long h) { setHandle(h); }

    /**
     * Bind to {@code url} with default {@link H264DepayConfig}.
     *
     * @param url {@code rtp://host:port?pt=N} where {@code N} is the dynamic
     *     payload type (1..=127; 33 is rejected — use {@link org.tstrans.shells.DemuxReceiver} for MPEG-TS)
     * @return a bound {@code H264Receiver}
     * @throws RtpException one member per open-path cause:
     *     {@code MISSING_PAYLOAD_TYPE_PARAM} if {@code ?pt=} is absent,
     *     {@code URL} on a malformed URL or a rejected URL parameter (a
     *     {@code ?pt=} value outside 1..=127, or 33, lands here),
     *     {@code HOST_NOT_LITERAL} if the host is not a literal address,
     *     {@code IFACE_UNSUPPORTED} for an unsupported multicast interface,
     *     {@code IO} on the socket bind itself
     */
    public static H264Receiver listen(String url) throws RtpException {
        long h = nListen(url);
        if (h == 0) {
            throw new RtpException(RtpException.Kind.IO,
                "nListen returned 0 without throwing");
        }
        return new H264Receiver(h);
    }

    /**
     * Bind to {@code url} with an explicit {@link H264DepayConfig}.
     *
     * <p>The {@code ?pt=} value in the URL overrides {@code config.payloadType()}.
     *
     * @param url    {@code rtp://host:port?pt=N}
     * @param config depacketizer configuration
     * @return a bound {@code H264Receiver}
     * @throws RtpException as in {@link #listen(String)}
     */
    public static H264Receiver listen(String url, H264DepayConfig config) throws RtpException {
        // Build initialParameterSets as a flat byte[][] for the native.
        // Each element is one raw NALU (type 7 or 8).
        java.util.List<byte[]> psList = config.initialParameterSets();
        byte[][] psArr = psList.toArray(new byte[0][]);
        long h = nListenWithConfig(
            url,
            config.payloadType(),
            config.parameterSetInjection().ordinal(),
            psArr,
            config.maxAuBytes()
        );
        if (h == 0) {
            throw new RtpException(RtpException.Kind.IO,
                "nListenWithConfig returned 0 without throwing");
        }
        return new H264Receiver(h);
    }

    /**
     * Receive the next reassembled H.264 Access Unit.
     *
     * <p>Blocks until a packet arrives or EOS / error. The native call parks the
     * calling thread on kernel I/O; there is no GIL to release on the JVM side.
     *
     * @return the next {@link H264AccessUnit}, or {@code null} at EOS (the RTSP
     *     server ended the session — caller should exit the recv loop; a
     *     {@link #close()} on another thread never makes this call return
     *     {@code null}). After a fired cancel, the AUs already buffered
     *     (including the depacketizer's final AU, if it completes cleanly) are
     *     returned first.
     * @throws RtpException {@code CLOSED} once a cancel has fired and the
     *     buffered AUs are handed out, or when a {@link #close()} on another
     *     thread takes the receiver after this call reached it but before it
     *     started receiving; {@code IO} on a hard I/O error; {@code BACKPRESSURE}
     *     if a configured persistent recv deadline (the {@code ?recv_timeout=<ms>}
     *     URL knob) expires
     * @throws IllegalStateException if the receiver is already closed, also when
     *     a {@link #close()} on another thread began before this call reached the
     *     receiver
     * @see #recvAu(Integer) for a per-call deadline instead of (or on top of) a
     *     persistent one
     */
    public H264AccessUnit recvAu() throws RtpException {
        ensureOpen("H264Receiver is closed");
        return nRecvAu(peekHandle());
    }

    /**
     * Receive the next reassembled H.264 Access Unit, bounded by a per-call
     * deadline.
     *
     * @param timeoutMs milliseconds to wait for an AU; {@code null} blocks
     *     indefinitely, identically to {@link #recvAu()} (any persistent
     *     deadline armed by the {@code ?recv_timeout=<ms>} URL knob still
     *     applies in that case). A non-null value overrides the persistent
     *     deadline for this one call. A negative value behaves the same as
     *     {@code null} (blocks indefinitely) — there is no separate
     *     "immediate timeout" or rejected-argument case.
     * @return the next {@link H264AccessUnit}, or {@code null} at EOS (the RTSP
     *     server ended the session — caller should exit the recv loop; a
     *     {@link #close()} on another thread never makes this call return
     *     {@code null}). After a fired cancel, the AUs already buffered
     *     (including the depacketizer's final AU, if it completes cleanly) are
     *     returned first. A {@code null} return never means the deadline
     *     expired — expiry always throws {@code RtpException(BACKPRESSURE)}.
     * @throws RtpException {@code CLOSED} once a cancel has fired and the
     *     buffered AUs are handed out (a cancel wins over a deadline that is
     *     also due), or when a {@link #close()} on another thread takes the
     *     receiver after this call reached it but before it started receiving;
     *     {@code IO} on a hard I/O error; {@code BACKPRESSURE} if
     *     {@code timeoutMs} elapses, or (when {@code timeoutMs} is {@code null})
     *     a configured persistent recv deadline expires
     * @throws IllegalStateException if the receiver is already closed, also when
     *     a {@link #close()} on another thread began before this call reached the
     *     receiver
     */
    public H264AccessUnit recvAu(Integer timeoutMs) throws RtpException {
        ensureOpen("H264Receiver is closed");
        return nRecvAuTimeout(peekHandle(), timeoutMs == null ? -1L : (long) timeoutMs);
    }

    /**
     * Iterate reassembled H.264 Access Units. Each {@code hasNext()} blocks on
     * the next {@code recvAu()} until an AU arrives, EOS ({@code null}) is returned,
     * or an error occurs. A {@link RtpException} from the native pull is wrapped in
     * an unchecked {@link RuntimeException} (the {@code Iterator} contract forbids
     * checked exceptions) and thrown from {@code hasNext()}, or from {@code next()},
     * which calls it; a cancelled iteration ends that way too, with kind
     * {@code CLOSED}. A {@link #close()} on another thread also throws out of the
     * loop: a {@code RuntimeException} whose cause is an {@code RtpException} with
     * kind {@code CLOSED}, or an unwrapped {@link IllegalStateException} when the
     * next receive starts after the {@code close()} began (see {@link #recvAu()}).
     * Iteration ends normally ({@code hasNext()} returns {@code false}) only at the
     * end of the stream.
     *
     * <p>For-each shorthand:
     * <pre>{@code
     * try (H264Receiver rx = H264Receiver.listen("rtp://0.0.0.0:5004?pt=96")) {
     *     for (H264AccessUnit au : rx) {
     *         byte[] annexb = au.annexb();
     *         // ... decode or re-mux ...
     *     }
     * }
     * }</pre>
     */
    @Override
    public Iterator<H264AccessUnit> iterator() {
        return new Iterator<>() {
            private H264AccessUnit peeked;
            private boolean done;
            @Override public boolean hasNext() {
                if (done) return false;
                if (peeked != null) return true;
                try {
                    peeked = recvAu();
                } catch (RtpException e) {
                    throw new RuntimeException(e);
                }
                if (peeked == null) { done = true; return false; }
                return true;
            }
            @Override public H264AccessUnit next() {
                if (!hasNext()) throw new NoSuchElementException();
                H264AccessUnit au = peeked; peeked = null; return au;
            }
        };
    }

    /**
     * RFC 6184 depacketizer counters (AU counts, seq gaps, parameter-set updates).
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public H264DepayStats depayStats() {
        ensureOpen("H264Receiver is closed");
        return nDepayStats(peekHandle());
    }

    /**
     * RTP protocol-level statistics (malformed packet counter).
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public RtpStats rtpStats() {
        ensureOpen("H264Receiver is closed");
        return nRtpStats(peekHandle());
    }

    /**
     * Wire-level throughput statistics (bytes / packets received).
     *
     * <p>The same {@link SocketStats} the receiving shells' {@code socketStats()}
     * return, matching {@code tst_rtp::H264Receiver::socket_stats()}.
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("H264Receiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * The local address the UDP socket is bound to; empty for the TCP-interleaved (RTSP)
     * path, where no UDP socket exists.
     *
     * <p>Tests use this to discover the ephemeral port when the URL specifies
     * {@code :0}.
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public Optional<InetSocketAddress> localAddr() {
        ensureOpen("H264Receiver is closed");
        return Optional.ofNullable(NativeAddr.decode(nLocalAddr(peekHandle())));
    }

    /**
     * Obtain a cross-thread cancel handle. Obtain it BEFORE a blocking
     * {@link #recvAu()} call; calling {@link CancelHandle#cancel()} from another
     * thread wakes the parked recv within ~100&nbsp;ms; after any AUs already
     * buffered, it throws {@link RtpException} with kind {@code CLOSED}.
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("H264Receiver is closed");
        return ShellAdapters.cancelHandle(nCancelHandle(peekHandle()), "H264Receiver");
    }

    /**
     * Cancel any in-progress {@link #recvAu()} and free the underlying receiver.
     * Idempotent. Safe to call from another thread to stop a parked recv.
     *
     * <p><b>May block:</b> the cancel hook fires first (without taking the native
     * resource lock), then the close waits for the woken {@code recvAu()} to
     * release that lock. The parked recv polls its cancel flag at ~100&nbsp;ms
     * granularity, so for a receiver from {@link #listen(String)} a cross-thread
     * {@code close()} can take up to ~100&nbsp;ms (plus one packet-processing
     * interval) to return.
     *
     * <p>A receiver created via {@link RtspSession#intoH264Receiver()} owns the
     * session's RTSP client (the session wrapper was consumed at conversion), and this
     * then closes it as {@link RtspSession#close()} does: TEARDOWN with at most
     * 500&nbsp;ms for the reply, then up to 2&nbsp;s for the client's thread. A refused
     * or unanswered TEARDOWN is not reported. {@link #endReason()} then reads
     * {@code CLEAN_TEARDOWN} only when the server accepted the TEARDOWN within those
     * 500&nbsp;ms and no {@code recvAu()} was in progress; otherwise {@code CANCELLED}
     * (unless the session had already ended for its own reason).
     */
    @Override public void close() { super.close(); }

    /**
     * Why the receive session ended; empty if it hasn't ended yet
     * (or ended through a path this type doesn't instrument). Still readable
     * after {@link #close()} — the close path snapshots the reason before
     * the underlying native resource is freed. For a receiver made by
     * {@link RtspSession#intoH264Receiver()}, the reason after {@code close()} is
     * {@code CLEAN_TEARDOWN} only when the server accepted the close's TEARDOWN within
     * 500&nbsp;ms and no {@code recvAu()} was in progress; otherwise {@code CANCELLED}.
     *
     * <p><b>Never blocks.</b> While a {@link #recvAu()} is in flight on
     * another thread the reason is not final, and this answers empty
     * without waiting for that call (through 0.6.x it waited until the
     * receive returned). Read it after the recv loop has observed its
     * terminal outcome. Once closed, it returns the cached snapshot.
     *
     * <p><b>Cross-thread close race:</b> a concurrent call from another
     * thread while {@link #close()} is in flight may briefly observe an
     * empty answer before {@code close()} finishes — {@code peekHandle()}
     * reports closed as soon as {@code close()} claims the handle, before
     * the native teardown (which computes the snapshot) completes.
     */
    public Optional<EndReason> endReason() {
        long h = peekHandle();
        if (h == 0) return Optional.ofNullable(closedEndReason);
        return Optional.ofNullable(EndReason.fromWireValue(nEndReason(h)));
    }

    /**
     * Free-text detail for {@link #endReason()} — the message carried by
     * {@code KEEPALIVE_FAILED} / {@code TRANSPORT_FAILED} /
     * {@code PROTOCOL_ERROR}; empty for every other reason (including
     * "hasn't ended yet"). Still readable after {@link #close()}. Never
     * blocks, and has the same in-flight-receive and cross-thread-close-race
     * caveats as {@link #endReason()}.
     */
    public Optional<String> endDetail() {
        long h = peekHandle();
        if (h == 0) return Optional.ofNullable(closedEndDetail);
        return Optional.ofNullable(nEndDetail(h));
    }

    @Override protected void nativeClose(long h) {
        EndReasonSnapshot snapshot = nClose(h);
        if (snapshot == null) return;
        // Detail FIRST, reason SECOND: a reader that observes the (volatile)
        // reason write is then guaranteed (JMM happens-before) to also see
        // this detail write, which happened-before it in program order.
        closedEndDetail = snapshot.detail();
        closedEndReason = snapshot.reason();
    }

    // --- Natives ---

    private static native long nListen(String url) throws RtpException;
    private static native long nListenWithConfig(String url, int payloadType,
        int parameterSetInjection, byte[][] initialParameterSets,
        long maxAuBytes) throws RtpException;
    private static native H264AccessUnit nRecvAu(long handle) throws RtpException;
    private static native H264AccessUnit nRecvAuTimeout(long handle, long timeoutMs) throws RtpException;
    private static native H264DepayStats nDepayStats(long handle);
    private static native RtpStats nRtpStats(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native byte[] nLocalAddr(long handle);
    private static native long nCancelHandle(long handle);
    private static native int nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native EndReasonSnapshot nClose(long handle);
}
