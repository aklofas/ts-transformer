package org.tstrans.rtp;

import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DemuxerConfig;

/**
 * A live RTSP session (server in PLAY state). Drives the remaining control-plane
 * ({@link #pause()} / {@link #play()} / {@link #teardown()}) and consumes the RTP
 * data plane via {@link #intoDemuxReceiver()}. Mirrors tst-py
 * {@code tstrans.rtp.RtspSession}.
 *
 * <p><b>Thread safety:</b> the control methods are single-threaded — call them
 * from one thread. The one sanctioned cross-thread operation is {@link
 * RtspCancelHandle#cancel() cancel} on a {@link RtspCancelHandle} obtained from
 * {@link #cancelHandle()} BEFORE issuing a (potentially blocking) control call;
 * flipping it wakes a parked {@code pause}/{@code play}/{@code teardown} (which
 * then throws {@link RtspException}). {@link #close()} performs a best-effort
 * teardown; the cross-thread interrupt routes through {@link RtspCancelHandle}
 * (exactly like srt {@code Sender}/{@code Receiver}), not through {@code close()}.
 *
 * <p>The native handle is an {@link java.util.concurrent.atomic.AtomicLong}
 * registry key. {@code close()} claims it atomically with {@code getAndSet(0)};
 * the leased {@code HandleRegistry} guarantees no use-after-free/double-free for
 * any native call concurrent with {@code close()} — a racing call either runs or
 * throws a clean {@link IllegalStateException}.
 */
public final class RtspSession extends NativeHandle {
    static { NativeLoader.load(); }

    RtspSession(long h) { setHandle(h); }

    /** Send PAUSE. Server stops emitting RTP; the session stays valid for {@link #play()}. */
    public void pause() throws RtspException { ensureOpen("RtspSession is closed"); nPause(peekHandle()); }

    /** Send PLAY (resume after {@link #pause()}). */
    public void play() throws RtspException { ensureOpen("RtspSession is closed"); nPlay(peekHandle()); }

    /**
     * Send TEARDOWN. Closes the server session; subsequent {@code pause}/{@code play}
     * raise {@link RtspException}. Idempotent on the wrapper (a second call is a no-op).
     */
    public void teardown() throws RtspException { ensureOpen("RtspSession is closed"); nTeardown(peekHandle()); }

    /**
     * Obtain a cross-thread cancel handle. Obtain it BEFORE a blocking control call;
     * flipping {@link RtspCancelHandle#cancel()} from another thread breaks that call
     * out of blocking I/O.
     *
     * @throws IllegalStateException if the session is closed or already torn down
     */
    public RtspCancelHandle cancelHandle() {
        ensureOpen("RtspSession is closed");
        long h = nCancelHandle(peekHandle());
        if (h == 0) throw new IllegalStateException("RtspSession is torn down");
        return new RtspCancelHandle(h);
    }

    /**
     * RTCP-derived stats snapshot. Reserved — always returns a zeroed snapshot
     * until RTCP session stats land; see
     * {@code docs/project/deferred-features.md}. Mirrors tst-py's behavior.
     *
     * @throws IllegalStateException if the session is closed
     */
    public RtspStats stats() {
        ensureOpen("RtspSession is closed");
        return new RtspStats(0L, 0L, 0L, 0L, 0L, 0);
    }

    /** Consume the data plane with default demux options. See {@link #intoDemuxReceiver(DemuxerConfig)}. */
    public DemuxReceiver intoDemuxReceiver() throws RtspException {
        ensureOpen("RtspSession is closed");
        long h = nIntoDemuxReceiver(peekHandle(), false, 0, 0L, 0L, false, 0, 0L, false, 0L, false);
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nIntoDemuxReceiver returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    /**
     * Consume the session's RTP data plane and return a {@link DemuxReceiver} over
     * the demuxed {@code DemuxEvent} stream. The control methods remain usable
     * afterward (only the internal data-plane transport is consumed). Calling this
     * twice raises {@link RtspException} of kind {@code PROTOCOL}.
     *
     * <p><b>Lease semantics (contrast with {@link #intoH264Receiver()}):</b> this
     * method BORROWS the session handle — a sanctioned lease per the
     * {@code NativeHandle} subclass contract. Only the internal data-plane
     * transport moves into the returned receiver; this wrapper stays open, so
     * {@link #pause()}, {@link #play()}, {@link #teardown()} and {@link #close()}
     * remain usable (matching the Python binding). {@code intoH264Receiver()}
     * instead consumes the whole session — see its Javadoc for why. The Rust
     * layer enforces single use of the data plane: a second call throws
     * {@link RtspException} of kind {@code PROTOCOL}.
     *
     * @param demuxConfig demuxer configuration (must not be null; use
     *     {@link #intoDemuxReceiver()} for defaults)
     */
    public DemuxReceiver intoDemuxReceiver(DemuxerConfig demuxConfig) throws RtspException {
        ensureOpen("RtspSession is closed");
        long h = nIntoDemuxReceiver(peekHandle(), true,
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nIntoDemuxReceiver returned 0 without throwing");
        }
        return new DemuxReceiver(h);
    }

    /**
     * Consume this session and wrap its data plane in an {@link H264Receiver} for
     * iterating reassembled H.264 Access Units.
     *
     * <p>Raises {@link RtspException} of kind {@code PROTOCOL} when:
     * <ul>
     *   <li>this session was created via {@link RtspClient#connect(RtspClientConfig)}
     *       (not {@link RtspClient#connectH264}), or
     *   <li>{@link #intoDemuxReceiver()} has already consumed the data plane.
     * </ul>
     *
     * <p><b>Consumption semantics:</b> unlike {@link #intoDemuxReceiver()} (which
     * leaves this wrapper's control-plane methods usable), the H.264 path follows
     * the {@code NativeHandle} contract item 3 strictly: the session handle is
     * zeroed via {@link #consumeHandle()} BEFORE the fallible native work
     * (double-free safety), so this wrapper is closed on return — success
     * <em>or</em> failure — and subsequent control calls throw
     * {@link IllegalStateException} while {@link #close()} becomes a no-op. On
     * success the returned {@link H264Receiver} takes over the whole session:
     * the RTSP control connection (and keepalive) stays alive inside the
     * receiver, and {@link H264Receiver#close()} performs the best-effort
     * TEARDOWN. On failure the native tears the session down best-effort.
     *
     * <p><b>pause() / play() unavailable after conversion.</b> Because this method
     * consumes the session, {@link #pause()} and {@link #play()} are no longer
     * accessible once {@code intoH264Receiver()} returns. This differs from
     * {@link #intoDemuxReceiver()}, which leaves the control-plane methods open,
     * and from the Python binding ({@code session.into_h264_receiver()} keeps the
     * session wrapper usable). If you need mid-stream pause/resume from Java, drive
     * the control plane before calling this method and keep the interaction
     * single-threaded.
     *
     * <p><b>Failure with a live DemuxReceiver.</b> If {@link #intoDemuxReceiver()}
     * previously consumed the data plane, this call still consumes and tears down the
     * session before throwing {@link RtspException}. Any live {@link DemuxReceiver}
     * backed by this session will reach EOS on its next iteration.
     *
     * @return an {@link H264Receiver} over the post-SETUP RTP data plane
     * @throws RtspException {@code PROTOCOL} if the session was not created by
     *     {@code connectH264}, or if the data plane has already been consumed
     *     (this wrapper is consumed even when the call fails)
     * @throws IllegalStateException if this session is already closed
     */
    public H264Receiver intoH264Receiver() throws RtspException {
        ensureOpen("RtspSession is closed");
        // Claim the registry id atomically BEFORE the fallible native
        // (consume-first — NativeHandle contract item 3, same shape as
        // srt Socket.intoSender()). The native takes the whole session slot out
        // of its registry: success moves the control plane into the H264Receiver;
        // failure tears the session down best-effort. Either way a subsequent
        // close() finds 0 and is a harmless no-op.
        long sessionH = consumeHandle();
        long h = nIntoH264Receiver(sessionH);
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nIntoH264Receiver returned 0 without throwing");
        }
        return new H264Receiver(h);
    }

    /** Whether {@link #teardown()} (or {@link #close()}) has fired. */
    public boolean isTornDown() {
        if (peekHandle() == 0) return true;
        return nIsTornDown(peekHandle());
    }

    /** Best-effort teardown, then free the native session. Idempotent. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native void nPause(long handle) throws RtspException;
    private static native void nPlay(long handle) throws RtspException;
    private static native void nTeardown(long handle) throws RtspException;
    private static native long nCancelHandle(long handle);
    private static native long nIntoDemuxReceiver(long handle, boolean withConfig,
        int strict, long pesCapPerPid, long pesCapTotal, boolean cfi, int av1,
        long auCellCap, boolean lenientPsi, long syncBufCap,
        boolean unwrapTimestamps) throws RtspException;
    private static native long nIntoH264Receiver(long handle) throws RtspException;
    private static native boolean nIsTornDown(long handle);
    private static native void nClose(long handle);
}
