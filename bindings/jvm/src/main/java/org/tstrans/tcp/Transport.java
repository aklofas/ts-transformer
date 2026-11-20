package org.tstrans.tcp;

import java.net.InetSocketAddress;
import java.util.Objects;
import org.tstrans.MuxException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.internal.NativeAddr;
import org.tstrans.mpegts.Av1CarriageMode;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.Sender;

/**
 * One connection accepted by a {@link Listener}. It stays open when the listener
 * closes.
 *
 * <p>{@link #intoReceiver()}, {@link #intoDemuxReceiver()}, {@link #intoSender()} and
 * {@link #intoMuxSender(MuxerConfig)} <strong>consume</strong> the connection: it moves into
 * the returned shell, and every
 * later call on this {@code Transport} throws {@code IllegalStateException} except
 * {@link #close()}, which does nothing. The shell throws
 * {@link org.tstrans.TcpException}. On a {@code tcps://} listener the TLS handshake runs
 * on the shell's first receive or send. While a peer that connects has not answered the
 * handshake, the two directions differ: a receive parks until the shell's cancel handle
 * fires or the shell is closed; each send that reaches the socket throws
 * {@code TcpException(BACKPRESSURE)} after about one 100&nbsp;ms poll tick, and the
 * sender stays usable, so retry or close it.
 *
 * <pre>{@code
 * try (Listener listener = Listener.open("tcp://0.0.0.0:0?listen=1");
 *      Receiver rx = listener.accept().intoReceiver()) {
 *     byte[] ts = rx.recvBytes();
 * }
 * }</pre>
 *
 * <p>The JVM counterpart of the {@code tstrans.tcp.Transport} that Python's
 * {@code tstrans.tcp.Listener} accepts, without its {@code send} and {@code recv}: convert
 * it into a shell to move bytes.
 */
public final class Transport extends NativeHandle {

    static { NativeLoader.load(); }

    /** Package-private: constructed by {@link Listener#accept()} only. */
    Transport(long h) { setHandle(h); }

    /**
     * The peer's address, captured at accept. It is the TCP source address: behind a
     * proxy, the proxy's. An IPv4 client of a listener bound to {@code [::]} is reported
     * as an {@link java.net.Inet4Address}. The address is built from its bytes, with no
     * name lookup.
     *
     * @return the peer's address
     * @throws IllegalStateException if the connection is closed
     */
    public InetSocketAddress peerAddr() {
        ensureOpen("Transport is closed");
        return NativeAddr.decode(nPeerAddr(peekHandle()));
    }

    /**
     * Consume the connection and return a {@link Receiver} over it: TS bytes in, packet
     * aligned. The {@code Transport} is unusable afterwards; it is consumed even if the
     * call throws.
     *
     * <p>A {@code tcps://} handshake runs on the first {@code recvBytes()}, which throws
     * its failure (a peer that is not speaking TLS: {@code TcpException(BROKEN)}). While
     * the peer has not answered the handshake, that call parks until the receiver's cancel
     * handle fires or the receiver is closed. The receiver reports no {@code localPort()}.
     *
     * @return a receiver owning the connection; it throws {@link org.tstrans.TcpException}
     * @throws IllegalStateException if the connection is closed or already consumed
     */
    public Receiver intoReceiver() {
        ensureOpen("Transport is closed");
        long conn = consumeHandle(); // consumed: moved into the Receiver
        return ShellAdapters.receiver(nIntoReceiver(conn));
    }

    /**
     * Consume the connection and return a {@link DemuxReceiver} over it with default
     * demuxer options. Same rules as {@link #intoReceiver()}: consumed even if the call
     * throws; a {@code tcps://} handshake runs on the first {@code recvEvent()}, which
     * parks until the cancel handle fires or the receiver is closed while the peer has not
     * answered it.
     *
     * @return a demux receiver owning the connection; it throws
     *     {@link org.tstrans.TcpException}
     * @throws IllegalStateException if the connection is closed or already consumed
     */
    public DemuxReceiver intoDemuxReceiver() {
        ensureOpen("Transport is closed");
        long conn = consumeHandle(); // consumed: moved into the DemuxReceiver
        return ShellAdapters.demuxReceiver(nIntoDemuxReceiver(conn));
    }

    /**
     * {@link #intoDemuxReceiver()} with explicit demuxer options. A {@code null} config,
     * or a config the native layer refuses, leaves this {@code Transport} open and
     * unconsumed, so a corrected retry can still convert it; the connection is claimed only
     * once the demux receiver holds it.
     *
     * @param demuxConfig the demuxer configuration
     * @return a demux receiver owning the connection; it throws
     *     {@link org.tstrans.TcpException}
     * @throws NullPointerException if {@code demuxConfig} is {@code null}
     * @throws IllegalStateException if the connection is closed or already consumed
     */
    public DemuxReceiver intoDemuxReceiver(DemuxerConfig demuxConfig) {
        ensureOpen("Transport is closed");
        Objects.requireNonNull(demuxConfig, "demuxConfig");
        int strict = demuxConfig.strictMode().code();
        long pesCapPerPid = demuxConfig.pesCapPerPid();
        long pesCapTotal = demuxConfig.pesCapTotal();
        boolean cfiTolerance = demuxConfig.cfiTolerance();
        int av1Carriage = demuxConfig.av1Carriage().code();
        long auCellCapPerPid = demuxConfig.auCellCapPerPid();
        boolean lenientPsi = demuxConfig.lenientPsiReassembly();
        long syncBufCap = demuxConfig.syncBufCap();
        boolean unwrapTimestamps = demuxConfig.unwrapTimestamps();
        // Claimed after the native takes the connection: the native builds the demuxer
        // config first, so a refusal leaves the connection registered under this key.
        long h = nIntoDemuxReceiverWithConfig(peekHandle(), strict, pesCapPerPid, pesCapTotal,
            cfiTolerance, av1Carriage, auCellCapPerPid, lenientPsi, syncBufCap, unwrapTimestamps);
        consumeHandle();
        return ShellAdapters.demuxReceiver(h);
    }

    /**
     * Consume the connection and return a {@link Sender} over it: pre-muxed TS bytes
     * out. The {@code Transport} is unusable afterwards; it is consumed even if the call
     * throws.
     *
     * <p>A {@code tcps://} handshake runs on the first send that reaches the socket. While
     * the peer has not answered the handshake, each {@code sendBytes} or {@code flush} that
     * reaches the socket throws {@code TcpException(BACKPRESSURE)} after about one
     * 100&nbsp;ms poll tick instead of parking; the sender stays usable, so retry or close
     * it.
     *
     * @return a sender owning the connection; it throws {@link org.tstrans.TcpException}
     * @throws IllegalStateException if the connection is closed or already consumed
     */
    public Sender intoSender() {
        ensureOpen("Transport is closed");
        long conn = consumeHandle(); // consumed: moved into the Sender
        return ShellAdapters.sender(nIntoSender(conn));
    }

    /**
     * Consume the connection and return a {@link MuxSender} over it: elementary streams in,
     * muxed by {@code cfg} into TS. The muxer configuration is built and checked before the
     * connection is taken, so a {@code null} config or one the muxer refuses leaves this
     * {@code Transport} open and unconsumed; a corrected retry can still convert it.
     *
     * <p>A {@code tcps://} handshake runs on the first send that reaches the socket, as
     * for {@link #intoSender()}.
     *
     * @param cfg the muxer program configuration
     * @return a mux sender owning the connection; it throws {@link org.tstrans.TcpException}
     * @throws NullPointerException if {@code cfg} is {@code null}
     * @throws IllegalStateException if the connection is closed or already consumed
     * @throws MuxException {@code CONFIG_INVALID} if the muxer refuses {@code cfg}
     */
    public MuxSender intoMuxSender(MuxerConfig cfg) throws MuxException {
        ensureOpen("Transport is closed");
        Objects.requireNonNull(cfg, "cfg");
        // The native's AV1 carriage codes: 0 = MPEG-2 TS binding, 1 = raw OBUs.
        int av1 = cfg.av1Carriage() == Av1CarriageMode.MPEG2_TS_BINDING ? 0 : 1;
        // Claimed after the native takes the connection, as intoDemuxReceiver(DemuxerConfig).
        long h = nIntoMuxSender(peekHandle(), cfg.programNumber(), cfg.pmtPid(), cfg.pcrPid(),
            cfg.pcrIntervalMs(), cfg.pcrLeadMs(), cfg.psiIntervalMs(), cfg.bufferPackets(),
            av1, cfg.streamPids(), cfg.streamKinds(), cfg.streamCodecs(),
            cfg.streamTypeCodes(), cfg.streamCarriesPts(), cfg.dataDescBytes(), cfg.dataDescLens());
        consumeHandle();
        return ShellAdapters.muxSender(h);
    }

    /** Close the connection. Idempotent; does nothing once the connection is consumed. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native byte[] nPeerAddr(long handle);

    private static native void nClose(long handle);

    private static native long nIntoReceiver(long handle);

    private static native long nIntoDemuxReceiver(long handle);

    private static native long nIntoDemuxReceiverWithConfig(long handle, int strict,
        long pesCapPerPid, long pesCapTotal, boolean cfiTolerance, int av1Carriage,
        long auCellCapPerPid, boolean lenientPsi, long syncBufCap, boolean unwrapTimestamps);

    private static native long nIntoSender(long handle);

    private static native long nIntoMuxSender(long handle, int programNumber, int pmtPid,
        int pcrPid, int pcrIntervalMs, int pcrLeadMs, int psiIntervalMs, int bufferPackets,
        int av1Carriage, int[] streamPids, int[] streamKinds, int[] streamCodecs,
        int[] streamTypeCodes, boolean[] streamCarriesPts, byte[] dataDescBytes,
        int[] dataDescLens) throws MuxException;
}
