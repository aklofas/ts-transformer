package org.tstrans.rtp;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;

/**
 * Raw RTP transport (receiving side) — wraps {@code tst_rtp::RtpRecvTransport}. Binds to {@code url}
 * (literal IP:port); for multicast URLs the group is joined automatically.
 * {@link #recv()} returns the TS payload of one datagram (12-byte RTP header
 * already stripped) and blocks until a packet arrives or a cancel fires.
 *
 * <p><b>Thread safety:</b> a single {@code RecvTransport} is NOT thread-safe; use one
 * per thread. Cross-thread stop = {@link #cancelHandle()}{@code .cancel()}.
 *
 * <p>Mirrors {@code tstrans.rtp.RecvTransport} in tst-py. The TS shell over an
 * {@code rtp://} URL is {@link org.tstrans.shells.Receiver}.
 */
public class RecvTransport extends NativeHandle {
    static { NativeLoader.load(); }

    // Populated by nativeClose from nClose's close-time snapshot (see
    // endReason()/endDetail()'s javadoc for why: once closed, peekHandle()
    // is 0 and there is no handle left to pass to a live-getter native).
    private volatile org.tstrans.shells.EndReason closedEndReason;
    private volatile String closedEndDetail;

    /** Wrap a native handle returned by {@link #openHandle(String)}. */
    protected RecvTransport(long h) { setHandle(h); }

    /** Bind a receiver to {@code url} ({@code rtp://host:port}, unicast or
     *  multicast). The receive buffer sizes itself to the transport's
     *  deliverable ceiling; {@code ?pkt_size=} on a receiver URL is rejected.
     *
     * @throws RtpException one member per open-path cause: {@code URL} on a
     *     malformed URL or a rejected URL parameter (a {@code ?pkt_size=} on a
     *     receiver URL lands here), {@code PAYLOAD_TYPE_PARAM} if the URL
     *     carries {@code ?pt=} (that is elementary RTP — use
     *     {@link H264Receiver}), {@code HOST_NOT_LITERAL} if the host is not a
     *     literal address, {@code IFACE_UNSUPPORTED} for an unsupported
     *     multicast interface, {@code IO} on the socket bind itself
     */
    public static RecvTransport fromUrl(String url) throws RtpException {
        return new RecvTransport(openHandle(url));
    }

    /**
     * Open the native transport and return its handle, for a subclass's own
     * factory. Same contract as {@link #fromUrl(String)}.
     *
     * @return the native handle, to pass to the constructor
     * @throws RtpException as {@link #fromUrl(String)}
     */
    protected static long openHandle(String url) throws RtpException {
        long h = nFromUrl(url);
        if (h == 0) {
            throw new RtpException(RtpException.Kind.IO, "nFromUrl returned 0 without throwing");
        }
        return h;
    }

    /**
     * Receive one TS payload chunk. Blocks until a packet arrives or a cancel fires.
     *
     * @throws IllegalStateException if the receiver is closed
     * @throws RtpException {@code CLOSED} if a cancel fired; {@code IO} otherwise;
     *     {@code BACKPRESSURE} if a configured persistent recv deadline (the
     *     {@code ?recv_timeout=<ms>} URL knob) expires
     * @see #recv(Integer) for a per-call deadline instead of (or on top of) a
     *     persistent one
     */
    public byte[] recv() throws RtpException {
        ensureOpen("RecvTransport is closed");
        return nRecv(peekHandle());
    }

    /**
     * Receive one TS payload chunk, bounded by a per-call deadline.
     *
     * @param timeoutMs milliseconds to wait for a packet; {@code null} blocks
     *     indefinitely, identically to {@link #recv()} (any persistent
     *     deadline armed by the {@code ?recv_timeout=<ms>} URL knob still
     *     applies in that case). A non-null value overrides the persistent
     *     deadline for this one call. A negative value behaves the same as
     *     {@code null} (blocks indefinitely) — there is no separate
     *     "immediate timeout" or rejected-argument case.
     * @throws IllegalStateException if the receiver is closed
     * @throws RtpException {@code CLOSED} if a cancel fired; {@code IO}
     *     otherwise; {@code BACKPRESSURE} if {@code timeoutMs} elapses, or
     *     (when {@code timeoutMs} is {@code null}) a configured persistent
     *     recv deadline expires
     */
    public byte[] recv(Integer timeoutMs) throws RtpException {
        ensureOpen("RecvTransport is closed");
        return nRecvTimeout(peekHandle(), timeoutMs == null ? -1L : (long) timeoutMs);
    }

    /** Snapshot of wire-level statistics (never null in normal operation). */
    public org.tstrans.shells.SocketStats socketStats() {
        ensureOpen("RecvTransport is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link org.tstrans.shells.CancelHandle#cancel()}
     * wakes a thread parked in {@link #recv}; that call throws
     * {@code RtpException(CLOSED)}.
     */
    public org.tstrans.shells.CancelHandle cancelHandle() {
        ensureOpen("RecvTransport is closed");
        return ShellAdapters.cancelHandle(nCancelHandle(peekHandle()));
    }

    /**
     * Why the receive session ended, or {@code null} if it hasn't ended yet
     * (or ended through a path this type doesn't instrument). Still readable
     * after {@link #close()} — the close path snapshots the reason before
     * the underlying native resource is freed.
     *
     * <p><b>Never blocks:</b> the reason is read off a lock-free cell captured
     * when the receiver was opened, so it answers while a recv is parked on
     * another thread. Once closed, it returns the cached snapshot.
     *
     * <p><b>Cross-thread close race:</b> a concurrent call from another
     * thread while {@link #close()} is in flight may briefly observe
     * {@code null} before {@code close()} finishes — {@code peekHandle()}
     * reports closed as soon as {@code close()} claims the handle, before
     * the native teardown (which computes the snapshot) completes.
     */
    public org.tstrans.shells.EndReason endReason() {
        long h = peekHandle();
        if (h == 0) return closedEndReason;
        return org.tstrans.shells.EndReason.fromWireValue(nEndReason(h));
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

    /** Close the transport. Idempotent. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) {
        org.tstrans.shells.EndReason.Snapshot snapshot = nClose(h);
        if (snapshot == null) return;
        // Detail FIRST, reason SECOND: a reader that observes the (volatile)
        // reason write is then guaranteed (JMM happens-before) to also see
        // this detail write, which happened-before it in program order.
        closedEndDetail = snapshot.detail();
        closedEndReason = snapshot.reason();
    }

    private static native long   nFromUrl(String url) throws RtpException;
    private static native byte[] nRecv(long handle) throws RtpException;
    private static native byte[] nRecvTimeout(long handle, long timeoutMs) throws RtpException;
    private static native org.tstrans.shells.SocketStats nSocketStats(long handle);
    private static native long   nCancelHandle(long handle);
    private static native int    nEndReason(long handle);
    private static native String nEndDetail(long handle);
    private static native org.tstrans.shells.EndReason.Snapshot nClose(long handle);
}
