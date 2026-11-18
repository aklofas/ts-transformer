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
 * <p><b>Thread safety:</b> {@link #cancelHandle()} returns at once from any thread, and
 * {@link #close()} may be called from any thread: it cancels a control call in progress
 * there and closes once that call has let go. The control calls ({@link #pause()}, {@link #play()}, {@link #teardown()})
 * run one at a time: a second one waits for the call in progress, up to that call's
 * request timeout (10&nbsp;s). {@link RtspCancelHandle#cancel() cancel} on a handle wakes
 * a control call in progress, which then throws {@link RtspException} of kind
 * {@code CLOSED}. A {@code close()} that overlaps a control call in progress on another
 * thread cancels it (that call throws {@code CLOSED}), so the session then ends as a
 * cancel, not as a clean teardown: a live receiver ends {@code RtpException(CLOSED)}.
 * A {@code pause()} / {@code play()} that reached the native session before a
 * {@code close()} began, and loses it to that close, throws {@code CLOSED}; one that
 * starts after the close began throws {@link IllegalStateException}. A {@code close()}
 * made while another {@code close()} is running returns at once without waiting for it.
 *
 * <p><b>{@code teardown()} or {@code close()}:</b> {@link #teardown()} sends TEARDOWN,
 * waits for the reply up to the request timeout (10&nbsp;s) and throws what the server
 * refused; the session object stays open for {@code close()}. {@link #close()} is silent
 * and bounded: TEARDOWN with at most 500&nbsp;ms for the reply (none is sent after
 * {@code teardown()}), then up to 2&nbsp;s for the client's thread to end. A caller who
 * needs a slow server's reply awaited calls {@code teardown()} first.
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
     * A 2xx reply, or {@code 454 Session Not Found} (the session is already gone), returns
     * normally; a TEARDOWN the server refuses throws {@link RtspException} of the status's
     * kind ({@code PROTOCOL}, or {@code NOT_FOUND} for a 404), and the session still counts
     * as torn down. A first 401 is answered once with the credentials, unless it carries no
     * {@code WWW-Authenticate} header ({@code PROTOCOL}), the client has no credentials
     * ({@code AUTH_FAILED}) or its challenge offers no scheme the client may answer
     * ({@code AUTH_REQUIRED}); a 401 to that retry throws {@code AUTH_FAILED}.
     * The reply wait is the request timeout (10&nbsp;s). The session object stays open:
     * call {@link #close()} afterwards, which then sends no second TEARDOWN. A
     * {@code close()} alone waits at most 500&nbsp;ms for the reply and swallows a
     * refusal. A {@code teardown()} that overlaps a {@link #close()} on another thread
     * throws {@code CLOSED} when the close cancelled it, and returns normally when the
     * close had already ended the client: the session is torn down, and the close's own
     * TEARDOWN result is not reported (unlike {@code pause()} / {@code play()}, which
     * throw {@code CLOSED} there).
     */
    public void teardown() throws RtspException { ensureOpen("RtspSession is closed"); nTeardown(peekHandle()); }

    /**
     * Obtain a cross-thread cancel handle. Answers at once from any thread, also while
     * a control call is parked on another one; flipping
     * {@link RtspCancelHandle#cancel()} breaks that call out of blocking I/O, and it
     * throws {@link RtspException} of kind {@code CLOSED}. A handle is returned after
     * {@link #teardown()} too. All handles of one session share one flag, which
     * {@link #close()} sets.
     *
     * @throws IllegalStateException if the session is closed
     */
    public RtspCancelHandle cancelHandle() {
        ensureOpen("RtspSession is closed");
        long h = nCancelHandle(peekHandle());
        if (h == 0) throw new IllegalStateException("RtspSession is closed");
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
        return new RtspStats(0L, 0L, 0L, 0);
    }

    /** Consume the data plane with default demux options. See {@link #intoDemuxReceiver(DemuxerConfig)}. */
    public org.tstrans.shells.DemuxReceiver intoDemuxReceiver() throws RtspException {
        ensureOpen("RtspSession is closed");
        long h = nIntoDemuxReceiver(peekHandle(), false, 0, 0L, 0L, false, 0, 0L, false, 0L, false);
        if (h == 0) {
            throw new RtspException(RtspException.Kind.PROTOCOL,
                "nIntoDemuxReceiver returned 0 without throwing");
        }
        return ShellAdapters.demuxReceiver(h);
    }

    /**
     * Consume the session's RTP data plane and return a {@link org.tstrans.shells.DemuxReceiver} over
     * the demuxed {@code DemuxEvent} stream. The control methods remain usable
     * afterward (only the internal data-plane transport is consumed). Calling this
     * twice raises {@link RtspException} of kind {@code CLOSED}.
     *
     * <p><b>Lease semantics (contrast with {@link #intoH264Receiver()}):</b> this
     * method BORROWS the session handle — a sanctioned lease per the
     * {@code NativeHandle} subclass contract. Only the internal data-plane
     * transport moves into the returned receiver; this wrapper stays open, so
     * {@link #pause()}, {@link #play()}, {@link #teardown()} and {@link #close()}
     * remain usable (matching the Python binding). {@code intoH264Receiver()}
     * instead consumes the whole session — see its Javadoc for why. The Rust
     * layer enforces single use of the data plane: a second call throws
     * {@link RtspException} of kind {@code CLOSED}.
     *
     * @param demuxConfig demuxer configuration (must not be null; use
     *     {@link #intoDemuxReceiver()} for defaults)
     */
    public org.tstrans.shells.DemuxReceiver intoDemuxReceiver(DemuxerConfig demuxConfig) throws RtspException {
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
        return ShellAdapters.demuxReceiver(h);
    }

    /**
     * Consume this session and wrap its data plane in an {@link H264Receiver} for
     * iterating reassembled H.264 Access Units.
     *
     * <p>Raises {@link RtspException}:
     * <ul>
     *   <li>of kind {@code CLOSED} when {@link #intoDemuxReceiver()} has already
     *       consumed the data plane — checked first, so this holds for a session
     *       from either constructor;
     *   <li>of kind {@code PROTOCOL} when a fresh session was created via
     *       {@link RtspClient#connect(RtspClientConfig)} (not
     *       {@link RtspClient#connectH264}).
     * </ul>
     * A second {@code intoH264Receiver()} on this (already consumed) wrapper
     * throws {@link IllegalStateException}, the JVM closed-handle convention,
     * not {@link RtspException}.
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
     * receiver, and {@link H264Receiver#close()} closes the client as
     * {@link #close()} does. On failure the session is closed the same way.
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
     * previously consumed the data plane, this call still consumes the session before
     * throwing {@link RtspException}: it closes the session as {@link #close()} does
     * (TEARDOWN with at most 500&nbsp;ms for the reply, then up to 2&nbsp;s for the
     * client's thread). A live TCP-interleaved
     * {@link org.tstrans.shells.DemuxReceiver} backed by this session then ends after the
     * events already queued: with end of stream (iteration ends, {@code endReason()} is
     * {@code CLEAN_TEARDOWN}) when the server accepted the TEARDOWN within those
     * 500&nbsp;ms on a session whose cancel handle had not fired, and with
     * {@code RtpException(CLOSED)} when it refused it or did not answer in time, or when
     * the session's cancel handle had fired before the TEARDOWN was sent, whatever the
     * server then does. A session that had already ended for its own reason keeps that
     * reason and that outcome.
     *
     * @return an {@link H264Receiver} over the post-SETUP RTP data plane
     * @throws RtspException {@code CLOSED} if the data plane has already been
     *     consumed; {@code PROTOCOL} if a fresh session was not created by
     *     {@code connectH264} (this wrapper is consumed even when the call fails)
     * @throws IllegalStateException if this session is already closed or
     *     consumed (including by an earlier {@code intoH264Receiver()})
     */
    public H264Receiver intoH264Receiver() throws RtspException {
        ensureOpen("RtspSession is closed");
        // Claim the registry id atomically BEFORE the fallible native
        // (consume-first — NativeHandle contract item 3, same shape as
        // srt Socket.intoSender()). The native takes the whole session slot out
        // of its registry: success moves the control plane into the H264Receiver;
        // failure closes the session as close() does (a TEARDOWN bounded at
        // 500 ms, then the wait for the client's thread), except on a poisoned
        // lock, where the client is only dropped. Either way a subsequent
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

    /**
     * End the session and its client, then free the native session. Silent and
     * bounded: TEARDOWN with at most 500&nbsp;ms for the reply (none is sent after
     * {@link #teardown()}), then up to 2&nbsp;s for the client's thread, which closes the
     * control connection; a refused or unanswered TEARDOWN is not reported
     * ({@code teardown()} is the call that reports). Idempotent: a {@code close()} made
     * while another is running returns at once without waiting for it. Called from
     * another thread while a {@code pause()} / {@code play()} / {@code teardown()} is in
     * progress, it cancels that call first ({@code RtspException(CLOSED)}), then closes
     * the cancelled session: the TEARDOWN's reply is not awaited, and a live receiver
     * ends {@code RtpException(CLOSED)} whatever the server answers. An idle
     * {@code close()} does not cancel first. Handles from {@link #cancelHandle()} read
     * cancelled afterwards.
     *
     * <p>A live TCP-interleaved {@link org.tstrans.shells.DemuxReceiver} from
     * {@link #intoDemuxReceiver()} ends with end of stream when the server accepts the
     * TEARDOWN within those 500&nbsp;ms on a session whose cancel handle had not fired,
     * and with {@code RtpException(CLOSED)} when the server refuses it or does not answer
     * in time (call {@code teardown()} first to wait longer), or the session's cancel
     * handle had fired before the TEARDOWN was sent, whatever the server then does. A
     * session that had already ended for its own reason keeps that reason and that
     * outcome. A session keeps one end reason: open a new session for each stream.
     */
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
