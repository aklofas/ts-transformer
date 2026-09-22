package org.tstrans.rtp;

import java.util.Iterator;
import java.util.NoSuchElementException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;

/**
 * Blocking H.264-over-RTP receiver. Wraps {@code tst_rtp::H264Receiver}.
 * Mirrors tst-py {@code tstrans.rtp.H264Receiver}.
 *
 * <h3>Constructing</h3>
 *
 * <p>{@link #listen(String)} / {@link #listen(String, H264DepayConfig)} — bind a
 * UDP socket to an {@code rtp://host:port?pt=N} URL and return a ready receiver.
 * The {@code ?pt=} query parameter is required (range 1..=127; value 33 is
 * rejected — use {@link DemuxReceiver} for MPEG-TS streams).
 *
 * <h3>Receiving</h3>
 *
 * <p>{@link #recvAu()} blocks until a complete H.264 Access Unit is reassembled
 * or EOS. Returns {@link H264AccessUnit} on success, {@code null} at EOS (clean
 * close or RTSP teardown), or throws {@link RtpException} on transport failure.
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
 *   <li>{@link #socketStats()} — wire-level throughput (bytes/packets received).
 *       Note: unlike {@link DemuxReceiver#stats()}, this returns {@link SocketStats}
 *       directly (not wrapped in a {@code TransportStats}). The Rust/Python surface
 *       has the same asymmetry — {@code H264Receiver.socket_stats()} returns a bare
 *       {@code SocketStats}; {@code DemuxReceiver.stats()} returns a combined view.
 *   <li>{@link #rtpStats()} — protocol-level anomaly counter (malformed packets).
 *   <li>{@link #depayStats()} — RFC 6184 depacketizer internals (AU counts, seq
 *       gaps, parameter-set updates).
 * </ul>
 *
 * <h3>Lifecycle</h3>
 *
 * <p>{@link #close()} fires the cancel handle and frees the native receiver.
 * Subsequent {@link #recvAu()} throws {@link IllegalStateException}. Idempotent.
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
    private volatile StreamEndReason closedEndReason;
    private volatile String closedEndDetail;

    H264Receiver(long h) { setHandle(h); }

    /**
     * Bind to {@code url} with default {@link H264DepayConfig}.
     *
     * @param url {@code rtp://host:port?pt=N} where {@code N} is the dynamic
     *     payload type (1..=127; 33 is rejected — use {@link DemuxReceiver} for MPEG-TS)
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
     * @return the next {@link H264AccessUnit}, or {@code null} at EOS (clean close
     *     or RTSP teardown — caller should exit the recv loop)
     * @throws RtpException {@code CLOSED} if the cancel handle was fired
     *     explicitly; {@code IO} on a hard I/O error; {@code BACKPRESSURE} if
     *     a configured persistent recv deadline (the {@code ?recv_timeout=<ms>}
     *     URL knob) expires
     * @throws IllegalStateException if the receiver is already closed
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
     * @return the next {@link H264AccessUnit}, or {@code null} at EOS (clean
     *     close or RTSP teardown — caller should exit the recv loop). A
     *     {@code null} return never means the deadline expired — expiry always
     *     throws {@code RtpException(BACKPRESSURE)}.
     * @throws RtpException {@code CLOSED} if the cancel handle was fired
     *     explicitly; {@code IO} on a hard I/O error; {@code BACKPRESSURE} if
     *     {@code timeoutMs} elapses, or (when {@code timeoutMs} is {@code null})
     *     a configured persistent recv deadline expires
     * @throws IllegalStateException if the receiver is already closed
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
     * checked exceptions).
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
     * <p><b>Asymmetry note:</b> this method returns {@link SocketStats} directly,
     * matching {@code tst_rtp::H264Receiver::socket_stats()} and tst-py's
     * {@code H264Receiver.socket_stats()}. The {@link DemuxReceiver} wrapper
     * returns {@link TransportStats} (a combined view). The difference reflects
     * the Rust API shape, not a binding inconsistency.
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("H264Receiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * Local address the UDP socket is bound to, as {@code "host:port"}. Returns
     * {@code null} for the TCP-interleaved (RTSP) path where no UDP socket exists.
     *
     * <p>Tests use this to discover the ephemeral port when the URL specifies
     * {@code :0}.
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public String localAddr() {
        ensureOpen("H264Receiver is closed");
        return nLocalAddr(peekHandle());
    }

    /**
     * Obtain a cross-thread cancel handle. Obtain it BEFORE a blocking
     * {@link #recvAu()} call; calling {@link CancelHandle#cancel()} from another
     * thread wakes the parked recv within ~100&nbsp;ms, causing it to return
     * {@code null} (EOS) rather than throwing.
     *
     * @throws IllegalStateException if the receiver is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("H264Receiver is closed");
        long h = nCancelHandle(peekHandle());
        if (h == 0) throw new IllegalStateException("H264Receiver: cancelHandle native returned 0");
        return new CancelHandle(h);
    }

    /**
     * Cancel any in-progress {@link #recvAu()} and free the underlying receiver.
     * Idempotent. Safe to call from another thread to stop a parked recv.
     *
     * <p><b>May block briefly:</b> the cancel hook fires first (without taking
     * the native resource lock), then the close waits for the woken
     * {@code recvAu()} to release that lock. The parked recv polls its cancel
     * flag at ~100&nbsp;ms granularity, so a cross-thread {@code close()} can
     * take up to ~100&nbsp;ms (plus one packet-processing interval) to return.
     *
     * <p>For receivers created via {@link RtspSession#intoH264Receiver()}, this
     * also issues a best-effort RTSP TEARDOWN — the receiver owns the session's
     * control plane (the session wrapper was consumed at conversion).
     */
    @Override public void close() { super.close(); }

    /**
     * Why the receive session ended, or {@code null} if it hasn't ended yet
     * (or ended through a path this arc doesn't instrument). Still readable
     * after {@link #close()} — the close path snapshots the reason before
     * the underlying native resource is freed.
     *
     * <p><b>Blocking:</b> while open, this takes the same internal resource
     * lock a parked {@link #recvAu()} holds — if a recv is currently in
     * flight on this receiver, {@code endReason()} blocks until it returns
     * (an AU, EOS, or an error). Call after the recv loop observes its
     * terminal outcome, or from the same thread driving the recv loop, to
     * avoid blocking. Once closed, this never blocks (returns the cached
     * snapshot).
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

    private static native long nListen(String url) throws RtpException;
    private static native long nListenWithConfig(String url, int payloadType,
        int parameterSetInjection, byte[][] initialParameterSets,
        long maxAuBytes) throws RtpException;
    private static native H264AccessUnit nRecvAu(long handle) throws RtpException;
    private static native H264AccessUnit nRecvAuTimeout(long handle, long timeoutMs) throws RtpException;
    private static native H264DepayStats nDepayStats(long handle);
    private static native RtpStats nRtpStats(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native String nLocalAddr(long handle);
    private static native long nCancelHandle(long handle);
    private static native int nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native EndReasonSnapshot nClose(long handle);
}
