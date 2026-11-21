package org.tstrans.rtp;

import java.util.Objects;
import java.util.Optional;
import java.util.concurrent.atomic.AtomicLong;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.RtpException;
import org.tstrans.RtspException;
import org.tstrans.internal.EndReasonSnapshot;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.shells.EndReason;

/**
 * A recording RTSP session: what {@link RtspClient#push} returns once the server has
 * accepted its ANNOUNCE, SETUP and RECORD. It sends media to the server interleaved on
 * the control connection.
 *
 * <p>A session announced as {@link PushDescription#mp2t()} takes whole TS packets
 * through {@link #sendTs}, or becomes a sender shell through {@link #intoMuxSender} or
 * {@link #intoSender}. One announced with a video track takes access units and KLV
 * units through {@link #sendVideo} and {@link #sendKlv}.
 *
 * <p><b>Blocking.</b> A data call returns once its packets are queued for the client's
 * thread, not once they are on the wire. The queue is bounded: against a server that
 * reads slowly a data call waits for room. The wait ends when room frees, within about
 * 100&nbsp;ms of a cancel ({@link #cancelHandle()}, from any thread), when the session
 * ends, and when the local socket has taken no byte for 5&nbsp;s (the server reads
 * nothing), which ends the connection. That is not a bound on the whole wait: a server
 * that keeps reading a few bytes within every 5&nbsp;s holds the call for as long as it
 * does so, and the cancel handle is the way out. {@link #pause()}, {@link #teardown()}
 * and {@link #close()} first wait the same way, with no total bound either, for the
 * queued media to be written.
 *
 * <p><b>Thread safety.</b> The data and control calls run one at a time: a call made
 * while another is in progress on another thread waits for it. {@link #cancelHandle()},
 * {@link #isSessionAlive()}, {@link #sessionId()}, {@link #stats()},
 * {@link #endReason()}, {@link #endDetail()} and {@link #closeOutcome()} answer at once
 * from any thread, also while a send is waiting; the cancel handle is the way to end a
 * waiting send.
 * {@link #intoMuxSender} and {@link #intoSender} never wait for a send, {@code pause()}
 * or {@code resume()} running on another thread: they throw
 * {@link IllegalStateException} and consume nothing. {@link #close()} and
 * {@link #teardown()} may be called from any thread: when a send, {@code pause()} or
 * {@code resume()} is running on another thread they fire the cancel first (that call
 * throws {@code CLOSED}), then close. A send, {@code pause()} or {@code resume()} only
 * waiting for the session behind another call is not cancelled; it then runs nothing
 * and throws {@code CLOSED}.
 *
 * <p><b>How it ends.</b> Once the cancel handle has fired, every data call throws
 * {@link RtpException} of kind {@code CLOSED} (the session stays open until it is
 * closed). When the server closed the connection, the connection failed or took no
 * byte for 5&nbsp;s, a keepalive ping was answered {@code 454}, or the server broke the
 * framing, a data call throws kind {@code BROKEN}. A call on a closed session throws
 * {@link IllegalStateException}, except {@link #stats()}, {@link #endReason()},
 * {@link #endDetail()}, {@link #closeOutcome()} and {@link #isSessionAlive()}, which
 * answer; a send that was
 * waiting for the session when a {@code close()} on another thread began throws
 * {@code RtpException(CLOSED)} and sends nothing.
 *
 * <p><b>A panic.</b> A panic inside a call (a bug in the library) poisons the session:
 * every later call that takes it, the data and control calls and the conversions,
 * throws {@code RtspException(CLOSED)}, {@link #isSessionAlive()} reads {@code false},
 * and {@link #close()} and {@link #teardown()} still end the push.
 *
 * <p><b>Ending without loss.</b> {@link #close()} and {@link #teardown()} write the
 * queued media before the TEARDOWN, and so do {@code MuxSender.finish()} after
 * {@link #intoMuxSender} and {@code Sender.finish()} after {@link #intoSender}. A
 * sender's {@code close()} alone cancels first and can drop frames queued a moment
 * before. {@link #teardown()} reports how the TEARDOWN went; after a sender's
 * {@code finish()} (which cannot) {@link #closeOutcome()} tells it: accepted, refused
 * with its status, timed out, or queued media lost to a session the server had
 * forgotten.
 *
 * <p><b>Pause.</b> After {@link #pause()} the data calls still queue and write their
 * packets; some servers end the session on a packet sent while it is paused.
 *
 * <p><b>Keepalive.</b> A server may end a session it reads nothing from for its own
 * timeout, which can be shorter than the session timeout it advertises (10&nbsp;s on
 * some servers). With {@link RtspClientConfig#keepalive()} (the default) the client
 * sends keepalive pings at half the advertised session timeout (60&nbsp;s when the
 * server names none), or at {@link RtspClientConfig.Builder#keepaliveIntervalMs} when
 * set: pings at the default cadence do not protect a push that goes silent against a
 * server whose own timeout is shorter, so keep media flowing or set an interval below
 * it. Without keepalive, a push that can stay silent for the server's timeout may be
 * ended.
 *
 * <p><b>Not closing.</b> A session that is never closed keeps its connection, the
 * client's thread and its keepalive pings, and the server keeps the recording
 * session, until the process ends: nothing closes it when it is collected. Close it,
 * for example in a try-with-resources block.
 */
public final class RtspPushSession extends NativeHandle {
    static { NativeLoader.load(); }

    private static final String CLOSED = "RtspPushSession is closed";

    // The key of this object's reader of the push's counters and end reason: kept past
    // a close or a conversion until close(), so stats() answers after both. 0 once freed.
    private final AtomicLong statsKey = new AtomicLong();
    // What the reader said last before it was freed.
    private volatile RtspPushStats lastStats;
    private volatile EndReasonSnapshot lastEnd;
    private volatile PushCloseOutcome lastOutcome;
    // Set once a conversion took the push: then this object's close() frees the reader.
    private volatile boolean converted;

    RtspPushSession(long h) {
        setHandle(h);
        statsKey.set(nStatsKey(h));
    }

    /**
     * Send whole TS packets as one RTP packet on an MPEG-TS session: {@code ts} is a
     * multiple of 188 bytes, at most 1&nbsp;316 (seven packets), and each packet starts
     * with the sync byte {@code 0x47}. The array is copied.
     *
     * @param ts the TS packets
     * @throws RtpException {@code TOO_LARGE} for more than 1&nbsp;316 bytes and
     *     {@code BROKEN} for a payload that is empty or not whole TS packets, both with
     *     nothing sent and the session left open; once the session is cancelled or has
     *     ended, as the class describes
     * @throws IllegalArgumentException on a session announced with a video track; nothing
     *     is sent
     * @throws RtspException {@code CLOSED} on a session a panic poisoned (see the class)
     * @throws IllegalStateException if the session is closed
     */
    public void sendTs(byte[] ts) throws RtpException, RtspException {
        ensureOpen(CLOSED);
        Objects.requireNonNull(ts, "ts");
        nSendTs(peekHandle(), ts);
    }

    /**
     * Send one video access unit. {@code annexB} is the access unit in Annex B form (NAL
     * units behind start codes); successive calls are in decoding order. A NAL unit over
     * 1&nbsp;400 bytes is fragmented. The parameter sets travel only where the access
     * unit carries them. The array is copied.
     *
     * @param annexB the access unit
     * @param pts its presentation time, in 90&nbsp;kHz ticks
     * @throws RtpException once the session is cancelled or has ended, as the class
     *     describes; a cancel or the session's end during the call can leave the access
     *     unit sent in part
     * @throws IllegalArgumentException on an MPEG-TS session, or for an access unit with
     *     no NAL unit; nothing is sent
     * @throws RtspException {@code CLOSED} on a session a panic poisoned (see the class)
     * @throws IllegalStateException if the session is closed
     */
    public void sendVideo(byte[] annexB, long pts) throws RtpException, RtspException {
        ensureOpen(CLOSED);
        Objects.requireNonNull(annexB, "annexB");
        nSendVideo(peekHandle(), annexB, pts);
    }

    /**
     * Send one KLV unit on a session announced with KLV. {@code klv} is the whole unit
     * (a local or universal set, without a metadata access unit header). A unit up to
     * 65&nbsp;523 bytes is one RTP packet; a larger one is split. The array is copied.
     *
     * @param klv the KLV unit
     * @param pts its presentation time, in 90&nbsp;kHz ticks, on the video's timeline
     * @throws RtpException once the session is cancelled or has ended, as the class
     *     describes
     * @throws IllegalArgumentException on a session without a KLV track, or for an empty
     *     unit; nothing is sent
     * @throws RtspException {@code CLOSED} on a session a panic poisoned (see the class)
     * @throws IllegalStateException if the session is closed
     */
    public void sendKlv(byte[] klv, long pts) throws RtpException, RtspException {
        ensureOpen(CLOSED);
        Objects.requireNonNull(klv, "klv");
        nSendKlv(peekHandle(), klv, pts);
    }

    /**
     * Pause the recording: a last sender report per track is queued, the call waits
     * until the queued media is written, then PAUSE is sent. {@link #resume()} records
     * again.
     *
     * @throws RtspException {@code CLOSED} when the cancel handle is set or fires during
     *     the wait or before the reply (PAUSE is not sent once the wait has seen it);
     *     {@code PROTOCOL} on a session that is already paused (nothing written) and for
     *     a reply other than {@code 2xx} (the session is left as it is); {@code IO},
     *     {@code TIMEOUT} and the other errors of a request
     * @throws IllegalStateException if the session is closed
     */
    public void pause() throws RtspException {
        ensureOpen(CLOSED);
        nPause(peekHandle());
    }

    /**
     * Send RECORD again on a paused session. It does not wait for queued media: a frame
     * sent while paused can reach the server after the RECORD.
     *
     * @throws RtspException {@code PROTOCOL} on a session that is not paused (nothing
     *     written) and for a reply other than {@code 2xx}; {@code CLOSED} once the cancel
     *     handle has fired; {@code IO}, {@code TIMEOUT} and the other errors of a request
     * @throws IllegalStateException if the session is closed
     */
    public void resume() throws RtspException {
        ensureOpen(CLOSED);
        nResume(peekHandle());
    }

    /**
     * End the session as {@link #close()} does, and report the TEARDOWN's result: a last
     * sender report per track is queued, the queued media is written, then TEARDOWN is
     * sent with one 500&nbsp;ms bound over writing it and its reply, and the client's
     * thread is given up to 2&nbsp;s to end. The session is closed afterwards, on every
     * outcome: later calls throw {@link IllegalStateException} and {@code close()} does
     * nothing; {@link #stats()} and {@link #endReason()} still answer. (The play
     * session's {@link RtspSession#teardown()} keeps its session open instead.) Like
     * {@code close()}, it cancels a send, {@code pause()} or {@code resume()} running on
     * another thread first. A send, {@code pause()} or {@code resume()} only waiting for
     * the session is not cancelled, and the teardown does not wait for it to run: it
     * runs nothing and throws {@code CLOSED}. Media the close had to discard is counted
     * in {@link RtspPushStats#framesDroppedAtClose()}; this is the call that reports such
     * a loss.
     *
     * @throws RtspException in this order: {@code CLOSED} on a cancelled session (the
     *     TEARDOWN is still written); otherwise {@code PROTOCOL} with status 454 when
     *     queued media was lost to an expired session; otherwise the TEARDOWN's own
     *     result: the kind of the status for a TEARDOWN the server refuses
     *     ({@code PROTOCOL}, or {@code NOT_FOUND} for a 404), {@code TIMEOUT} when no
     *     reply came within the bound, {@code IO} when the connection failed; and
     *     {@code CLOSED} when a conversion on another thread took the session first:
     *     nothing was torn down, the converted shell owns the push, and
     *     {@link #stats()} and {@link #endReason()} keep following it
     * @throws IllegalStateException if the session is closed
     */
    public void teardown() throws RtspException {
        ensureOpen(CLOSED);
        long h = consumeHandle();
        if (h == 0) throw new IllegalStateException(CLOSED);
        boolean took;
        try {
            took = nTeardown(h);
        } catch (RtspException e) {
            releaseStats();   // the TEARDOWN ran and was refused or failed
            throw e;
        }
        if (!took) {
            // A conversion took the push first: its new owner ends it, and the
            // statistics keep following it.
            throw new RtspException(RtspException.Kind.CLOSED,
                "RtspPushSession.teardown: the session was converted on another thread");
        }
        releaseStats();
    }

    /**
     * A handle that ends a waiting data call, {@link #pause()} or a close from any
     * thread; the call then throws {@code CLOSED}. Never waits: it answers at once,
     * also while a send is waiting, and is the way to end that send. A cancelled
     * session sends nothing more; closing it still writes TEARDOWN. Every handle of one
     * session shares one flag, which every close sets.
     *
     * @return a new cancel handle; close it when done
     * @throws IllegalStateException if the session is closed
     */
    public RtspCancelHandle cancelHandle() {
        ensureOpen(CLOSED);
        long h = nCancelHandle(peekHandle());
        if (h == 0) throw new IllegalStateException(CLOSED);
        return new RtspCancelHandle(h);
    }

    /**
     * The session's counters now. Never waits and never throws: it answers while a send
     * is waiting on another thread, and after {@link #close()} or {@link #teardown()}
     * with the counters as the session ended. After {@link #intoSender} or
     * {@link #intoMuxSender} it follows the push the sender now owns, until this
     * object's {@code close()}, which keeps the counters as they stood then.
     * {@link RtspPushStats#framesDroppedAtClose()} counts media a close discarded;
     * {@link #teardown()} is the call that reports such a loss.
     *
     * @return a snapshot of the counters
     */
    public RtspPushStats stats() {
        long k = statsKey.get();
        RtspPushStats s = k == 0 ? null : nStatsOf(k);
        return s != null ? s : lastStats;
    }

    /**
     * Why the session ended; empty while it has not. Same rule as {@link #stats()}: it
     * never waits and answers after a close, a teardown and a conversion.
     * {@code CLEAN_TEARDOWN} after a TEARDOWN the server accepted and after a server
     * that closed the connection; {@code CANCELLED} after the cancel handle fired (a
     * converted sender's {@code close()} cancels first).
     *
     * @return the end reason, or empty
     */
    public Optional<EndReason> endReason() {
        return Optional.ofNullable(end().reason());
    }

    /**
     * Free-text detail for {@link #endReason()}: the message carried by
     * {@code KEEPALIVE_FAILED}, {@code TRANSPORT_FAILED} and {@code PROTOCOL_ERROR};
     * empty otherwise. Same rule as {@link #stats()}.
     *
     * @return the detail, or empty
     */
    public Optional<String> endDetail() {
        return Optional.ofNullable(end().detail());
    }

    /**
     * How the push's first close ended; empty while no close has run. Same rule as
     * {@link #stats()}: it never waits and never throws, and answers after a close, a
     * teardown and a conversion (then it follows the push the sender owns, until this
     * object's {@code close()}).
     *
     * <p>The close is whichever ended the push first: this session's {@link #close()} or
     * {@link #teardown()}, or after a conversion the sender's {@code finish()} or
     * {@code close()}. A sender's {@code close()} fires the cancel first, so it reads
     * {@link PushCloseOutcome.Kind#CANCELLED}; its {@code finish()} leaves the TEARDOWN's
     * own case. A {@link #teardown()} that closed the push and threw reads the same case
     * it threw.
     *
     * @return the outcome, or empty
     */
    public Optional<PushCloseOutcome> closeOutcome() {
        long k = statsKey.get();
        PushCloseOutcome o = k == 0 ? null : nCloseOutcomeOf(k);
        return Optional.ofNullable(o != null ? o : lastOutcome);
    }

    private EndReasonSnapshot end() {
        long k = statsKey.get();
        EndReasonSnapshot e = k == 0 ? null : nEndOf(k);
        return e != null ? e : lastEnd;
    }

    // Keep what the reader says now, then free it; a reader racing this answers with
    // the kept values once the key is gone. Called only by the call that ended the
    // session (or, after a conversion, by close()), so the values kept are final.
    private void releaseStats() {
        long k = statsKey.get();
        if (k == 0) return;
        RtspPushStats s = nStatsOf(k);
        if (s != null) lastStats = s;
        EndReasonSnapshot e = nEndOf(k);
        if (e != null) lastEnd = e;
        PushCloseOutcome o = nCloseOutcomeOf(k);
        if (o != null) lastOutcome = o;
        if (statsKey.compareAndSet(k, 0)) nFreeStats(k);
    }

    /**
     * The session id the server named at SETUP. Answers at once while the session is
     * open; after {@link #close()} it throws, as a closed session's getters do except
     * {@link #stats()}, {@link #endReason()}, {@link #endDetail()} and
     * {@link #isSessionAlive()} (Python's {@code session_id} stays readable).
     *
     * @return the session id
     * @throws IllegalStateException if the session is closed
     */
    public String sessionId() {
        ensureOpen(CLOSED);
        return nSessionId(peekHandle());
    }

    /**
     * False once the session is known dead: the control connection ended, or a keepalive
     * ping was answered {@code 454}; also false once this session is closed, torn down or
     * converted into a sender shell.
     *
     * <p>Never waits and never throws: a call in progress on another thread (a send
     * waiting for a slow server, for example) counts as live, and the answer is
     * {@code true} while it runs, unless a close of this session has begun.
     *
     * @return whether the session is still alive
     */
    public boolean isSessionAlive() {
        long h = peekHandle();
        return h != 0 && nIsSessionAlive(h);
    }

    /**
     * Consume this MPEG-TS session into a {@link org.tstrans.shells.MuxSender} that muxes
     * {@code config}'s program and sends it through the session.
     * {@link org.tstrans.shells.MuxSender#finish()} writes the muxer's pending bytes and
     * the media queued in the session, then sends TEARDOWN. The sender's {@code close()}
     * cancels the session first, as it does for every transport: media still queued is
     * dropped and TEARDOWN is written without that wait. Call {@code finish()} before
     * {@code close()} for a lossless end; it does not report the TEARDOWN's answer,
     * {@link #closeOutcome()} on this session does.
     *
     * <p>Called while a send, {@code pause()} or {@code resume()} is running on another
     * thread (a send waiting for a slow server, for example), it does not wait: it throws
     * {@link IllegalStateException} and consumes nothing, and the session stays usable
     * (once a close of the session has begun it throws as on a closed session). A
     * refused muxer configuration, and a session announced with a video track, also
     * leave this session open and unconsumed. On success this session is closed, and a
     * send, {@code pause()} or {@code resume()} on another thread that was waiting for it
     * runs nothing and throws {@code CLOSED}; {@link #stats()} and {@link #endReason()}
     * then follow the push the sender owns.
     *
     * @param config the muxer program configuration
     * @return a {@code MuxSender} that owns the session
     * @throws MuxException {@code CONFIG_INVALID} if the muxer refuses the configuration
     * @throws RtspException {@code CLOSED} on a session a panic poisoned (see the class);
     *     nothing is consumed
     * @throws IllegalArgumentException on a session announced with a video track
     * @throws IllegalStateException if the session is closed, or while a send,
     *     {@code pause()} or {@code resume()} is running on another thread
     * @throws NullPointerException if {@code config} is {@code null}
     */
    public org.tstrans.shells.MuxSender intoMuxSender(MuxerConfig config)
            throws RtspException, MuxException {
        ensureOpen(CLOSED);
        Objects.requireNonNull(config, "config");
        // The native refuses an elementary session and builds the muxer config before
        // it takes the session, so a refusal leaves the session under this key; the key
        // is claimed only once the native has taken it.
        long h = nIntoMuxSender(peekHandle(), config.programNumber(), config.pmtPid(),
            config.pcrPid(), config.pcrIntervalMs(), config.pcrLeadMs(), config.psiIntervalMs(),
            config.bufferPackets(), config.av1Carriage().code(), config.streamPids(),
            config.streamKinds(), config.streamCodecs(), config.streamTypeCodes(),
            config.streamCarriesPts(), config.dataDescBytes(), config.dataDescLens());
        consumeHandle();
        converted = true;
        return ShellAdapters.muxSender(h);
    }

    /**
     * Consume this MPEG-TS session into a {@link org.tstrans.shells.Sender} that sends
     * pre-muxed TS bytes through it. The sender's {@code close()} cancels the session
     * first, as it does for every transport, then ends it: media still queued for the
     * server is dropped and TEARDOWN is written without that wait.
     *
     * <p>{@link org.tstrans.shells.Sender#finish()} ends it without that loss: it
     * flushes the sender, then the session writes its queue and sends TEARDOWN. Like
     * {@code MuxSender.finish()} it does not report the TEARDOWN's answer or a queue
     * the server's end dropped; {@link #closeOutcome()} on this session tells both.
     *
     * <p>Called while a send, {@code pause()} or {@code resume()} is running on another
     * thread, it does not wait: it throws {@link IllegalStateException} and consumes
     * nothing, and the session stays usable (once a close of the session has begun it
     * throws as on a closed session). A session announced with a video track is also
     * left open and unconsumed. On success this session is closed, and a send,
     * {@code pause()} or {@code resume()} on another thread that was waiting for it runs
     * nothing and throws {@code CLOSED}; {@link #stats()} and {@link #endReason()} then
     * follow the push the sender owns.
     *
     * @return a {@code Sender} that owns the session
     * @throws RtspException {@code CLOSED} on a session a panic poisoned (see the class);
     *     nothing is consumed
     * @throws IllegalArgumentException on a session announced with a video track
     * @throws IllegalStateException if the session is closed, or while a send,
     *     {@code pause()} or {@code resume()} is running on another thread
     */
    public org.tstrans.shells.Sender intoSender() throws RtspException {
        ensureOpen(CLOSED);
        // Claimed after the native has taken the session (see intoMuxSender).
        long h = nIntoSender(peekHandle());
        consumeHandle();
        converted = true;
        return ShellAdapters.sender(h);
    }

    /**
     * End the session and free it. Silent: a last sender report per track is queued,
     * the queued media is written (see "Blocking" above), then TEARDOWN with one
     * 500&nbsp;ms bound over writing it and its reply, then up to 2&nbsp;s for the
     * client's thread, which closes the control connection. A refused or unanswered
     * TEARDOWN is not reported ({@link #teardown()} is the call that reports).
     *
     * <p>Idempotent: a second call, and a call made while another close is running,
     * return at once. Called while a send, {@code pause()} or {@code resume()} is
     * running on another thread, it fires the cancel first: that call throws
     * {@code CLOSED}, what is still queued is dropped, and the TEARDOWN is written
     * without the wait. A send, {@code pause()} or {@code resume()} only waiting for the
     * session is not cancelled, and the close does not wait for it to run: it runs
     * nothing and throws {@code CLOSED}. Afterwards {@link #stats()} and
     * {@link #endReason()} keep the values the session ended with; on a session already
     * converted into a sender, this call only fixes those values.
     */
    @Override public void close() {
        super.close();
        if (converted) releaseStats();
    }

    @Override protected void nativeClose(long h) {
        if (nClose(h)) releaseStats();
    }

    // Test-only: the raw registry key, for the jni-test-hooks probes.
    long nativeHandleForTest() { return peekHandle(); }

    private static native void nSendTs(long handle, byte[] ts) throws RtpException, RtspException;
    private static native void nSendVideo(long handle, byte[] annexB, long pts) throws RtpException, RtspException;
    private static native void nSendKlv(long handle, byte[] klv, long pts) throws RtpException, RtspException;
    private static native void nPause(long handle) throws RtspException;
    private static native void nResume(long handle) throws RtspException;
    private static native boolean nTeardown(long handle) throws RtspException;
    private static native long nCancelHandle(long handle);
    private static native long nStatsKey(long handle);
    private static native RtspPushStats nStatsOf(long statsKey);
    private static native EndReasonSnapshot nEndOf(long statsKey);
    private static native PushCloseOutcome nCloseOutcomeOf(long statsKey);
    private static native void nFreeStats(long statsKey);
    private static native String nSessionId(long handle);
    private static native boolean nIsSessionAlive(long handle);
    private static native long nIntoMuxSender(long handle, int programNumber, int pmtPid,
        int pcrPid, int pcrIntervalMs, int pcrLeadMs, int psiIntervalMs, int bufferPackets,
        int av1Carriage, int[] streamPids, int[] streamKinds, int[] streamCodecs,
        int[] streamTypeCodes, boolean[] streamCarriesPts, byte[] dataDescBytes,
        int[] dataDescLens) throws RtspException, MuxException;
    private static native long nIntoSender(long handle) throws RtspException;
    private static native boolean nClose(long handle);
}
