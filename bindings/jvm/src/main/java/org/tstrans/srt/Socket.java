package org.tstrans.srt;

import java.util.Optional;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.SrtException;

/**
 * Low-level SRT socket handle. Returned by {@link Builder#connect()} and
 * {@link Listener#accept(Integer)}.
 *
 * <p>{@link #intoSender()} / {@link #intoReceiver()} each <strong>consume</strong>
 * this handle — the underlying native socket moves into the new wrapper and
 * this {@code Socket}'s {@link #close()} becomes a no-op. The Java field is zeroed
 * immediately after the JNI call returns, so no double-free is possible even if
 * an exception is thrown mid-sequence.
 *
 * <p>{@link #intoMuxSender(org.tstrans.mpegts.MuxerConfig)} and
 * {@link #intoDemuxReceiver()} likewise consume this handle.
 *
 * <pre>{@code
 * try (Socket s = new Builder("srt://127.0.0.1:9000").caller().connect()) {
 *     Sender sender = s.intoSender(); // Socket handle consumed here
 *     try (sender) {
 *         sender.sendBytes(tsData);
 *     }
 * }
 * }</pre>
 */
public final class Socket extends NativeHandle {

    static { NativeLoader.load(); }

    /** Package-private: constructed by Builder and Listener only. */
    Socket(long h) { setHandle(h); }

    /**
     * Consume this socket and produce a {@link Sender}.
     *
     * <p>The underlying {@code tst_srt::Socket} moves into the new Sender; subsequent
     * calls on {@code this} will throw {@code IllegalStateException("Socket is closed")}.
     *
     * @throws IllegalStateException if the socket is already closed
     * @throws SrtException on transport error
     */
    public Sender intoSender() throws SrtException {
        ensureOpen("Socket is closed");
        // Claim the registry id atomically before the native call (consume-first);
        // a concurrent close/consume that wins leaves us with 0 and the native call
        // throws IllegalStateException.
        long sock = consumeHandle(); // consumed — moved into the Sender
        long h = nIntoSender(sock);
        return new Sender(h);
    }

    /**
     * Consume this socket and produce a {@link Receiver}.
     *
     * <p>Same consumption semantics as {@link #intoSender()}.
     *
     * @throws IllegalStateException if the socket is already closed
     * @throws SrtException on transport error
     */
    public Receiver intoReceiver() throws SrtException {
        ensureOpen("Socket is closed");
        long sock = consumeHandle(); // consumed — moved into the Receiver
        long h = nIntoReceiver(sock);
        return new Receiver(h);
    }

    /**
     * Consume this socket and produce a {@link MuxSender} for the single-program
     * configuration {@code programConfig}.
     *
     * <p>Same consumption semantics as {@link #intoSender()}: the underlying
     * {@code tst_srt::Socket} moves into the new {@code MuxSender} and this
     * socket's handle is zeroed. The socket is consumed even if the muxer config
     * is rejected (the pending exception propagates from the native call).
     *
     * @param programConfig the muxer program configuration
     * @return a {@code MuxSender} owning this socket + a configured muxer
     * @throws IllegalStateException if the socket is already closed
     * @throws SrtException on transport error; {@link org.tstrans.MuxException}
     *     ({@code CONFIG_INVALID}) if the muxer config is rejected
     */
    public MuxSender intoMuxSender(org.tstrans.mpegts.MuxerConfig programConfig)
            throws SrtException, MuxException {
        ensureOpen("Socket is closed");
        // Claim the registry id BEFORE the native call (consume-first). nIntoMuxSender
        // closes the registry entry unconditionally but can still throw afterwards
        // (muxer-config rejection / MuxSender::new failure); claiming first means a
        // later close() finds 0 and is a clean no-op. (The registry already makes a
        // double-close idempotent, but consume-first keeps the contract explicit.)
        long sock = consumeHandle();
        long h = nIntoMuxSender(sock, programConfig.programNumber(), programConfig.pmtPid(),
            programConfig.pcrPid(), programConfig.pcrIntervalMs(), programConfig.psiIntervalMs(),
            programConfig.bufferPackets(), programConfig.av1Carriage().ordinal(),
            programConfig.streamPids(), programConfig.streamKinds(), programConfig.streamCodecs(),
            programConfig.streamTypeCodes(), programConfig.streamCarriesPts(),
            programConfig.dataDescBytes(), programConfig.dataDescLens());
        return new MuxSender(h);
    }

    /**
     * Consume this socket and produce a {@link DemuxReceiver} with default
     * demuxer options.
     *
     * <p>Same consumption semantics as {@link #intoSender()}: the underlying
     * {@code tst_srt::Socket} moves into the new {@code DemuxReceiver} and this
     * socket's handle is zeroed.
     *
     * @return a {@code DemuxReceiver} owning this socket + a default-configured demuxer
     * @throws IllegalStateException if the socket is already closed
     * @throws SrtException on transport error
     */
    public DemuxReceiver intoDemuxReceiver() throws SrtException {
        ensureOpen("Socket is closed");
        // Claim the registry id first (consume-first), uniform with intoMuxSender.
        long sock = consumeHandle();
        long h = nIntoDemuxReceiver(sock);
        return new DemuxReceiver(h);
    }

    /**
     * Consume this socket and produce a {@link DemuxReceiver} with the given
     * {@link org.tstrans.mpegts.DemuxerConfig}.
     *
     * <p>Same consumption semantics as {@link #intoDemuxReceiver()}.
     *
     * @param demuxConfig the demuxer configuration
     * @return a {@code DemuxReceiver} owning this socket + a configured demuxer
     * @throws IllegalStateException if the socket is already closed
     * @throws SrtException on transport error
     */
    public DemuxReceiver intoDemuxReceiver(org.tstrans.mpegts.DemuxerConfig demuxConfig)
            throws SrtException {
        ensureOpen("Socket is closed");
        // Consume-first (see intoDemuxReceiver()).
        long sock = consumeHandle();
        long h = nIntoDemuxReceiverWithConfig(sock,
            demuxConfig.strictMode().ordinal(), demuxConfig.pesCapPerPid(),
            demuxConfig.pesCapTotal(), demuxConfig.cfiTolerance(),
            demuxConfig.av1Carriage().ordinal(), demuxConfig.auCellCapPerPid(),
            demuxConfig.lenientPsiReassembly(), demuxConfig.syncBufCap(),
            demuxConfig.unwrapTimestamps());
        return new DemuxReceiver(h);
    }

    /**
     * Local bound address as a {@link HostPort} record. Useful when the URL
     * requested port 0 (kernel-assigned).
     *
     * @throws IllegalStateException if the socket is already closed
     * @throws SrtException if the socket handle is invalid ({@code IO}).
     */
    public HostPort localAddr() throws SrtException {
        ensureOpen("Socket is closed");
        return nLocalAddr(peekHandle());
    }

    /**
     * Peer (remote) address as a {@link HostPort} record.
     *
     * @throws SrtException if the socket is not connected or the handle is invalid ({@code IO}).
     */
    public HostPort peerAddr() throws SrtException {
        ensureOpen("Socket is closed");
        return nPeerAddr(peekHandle());
    }

    /**
     * Stream ID negotiated during the SRT handshake, if any.
     * Returns {@link Optional#empty()} if no stream ID was set.
     */
    public Optional<String> streamId() {
        ensureOpen("Socket is closed");
        return Optional.ofNullable(nStreamId(peekHandle()));
    }

    /** {@code true} while this socket still owns the native handle. */
    public boolean isAlive() {
        return peekHandle() != 0;
    }

    /**
     * Close the socket. Subsequent calls are no-ops. After close, {@link #intoSender()},
     * {@link #intoReceiver()}, {@link #localAddr()}, and {@link #peerAddr()} throw
     * {@code IllegalStateException}.
     */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    // --- Natives ---

    /**
     * Consume the Box&lt;Socket&gt; and build a Box&lt;Sender&gt;. Returns 0 and throws
     * SrtException on error. The Java caller MUST zero its own handle field
     * after this returns (regardless of success/failure) since the native handle
     * is consumed unconditionally.
     */
    private static native long nIntoSender(long handle) throws SrtException;

    private static native long nIntoReceiver(long handle) throws SrtException;

    /**
     * Consume the Box&lt;Socket&gt; and build a Box&lt;MuxSender&gt;. Returns 0 and
     * throws on error. The Java caller MUST zero its own handle field after this
     * returns (regardless of success/failure) since the native handle is
     * consumed unconditionally.
     */
    private static native long nIntoMuxSender(long handle, int programNumber, int pmtPid,
        int pcrPid, int pcrIntervalMs, int psiIntervalMs, int bufferPackets, int av1Carriage,
        int[] streamPids, int[] streamKinds, int[] streamCodecs, int[] streamTypeCodes,
        boolean[] streamCarriesPts,
        byte[] dataDescBytes, int[] dataDescLens) throws SrtException, MuxException;

    /**
     * Consume the Box&lt;Socket&gt; and build a Box&lt;DemuxReceiver&gt; (default demux
     * options). The Java caller MUST zero its own handle field before this call
     * (consume-first) since the native handle is consumed unconditionally.
     */
    private static native long nIntoDemuxReceiver(long handle) throws SrtException;

    private static native long nIntoDemuxReceiverWithConfig(long handle, int strict,
        long pesCapPerPid, long pesCapTotal, boolean cfi, int av1, long auCellCap,
        boolean lenientPsi, long syncBufCap, boolean unwrapTimestamps) throws SrtException;

    private static native HostPort nLocalAddr(long handle) throws SrtException;

    private static native HostPort nPeerAddr(long handle) throws SrtException;

    private static native String nStreamId(long handle);

    private static native void nClose(long handle);
}
