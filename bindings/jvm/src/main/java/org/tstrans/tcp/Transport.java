package org.tstrans.tcp;

import java.util.Objects;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.Sender;

/**
 * One connection accepted by a {@link Listener}. It stays open when the listener
 * closes.
 *
 * <p>{@link #intoReceiver()}, {@link #intoDemuxReceiver()} and {@link #intoSender()}
 * <strong>consume</strong> the connection: it moves into the returned shell, and every
 * later call on this {@code Transport} throws {@code IllegalStateException} except
 * {@link #close()}, which does nothing. The shell throws
 * {@link org.tstrans.TcpException}. On a {@code tcps://} listener the TLS handshake runs
 * on the shell's first receive or send; a peer that connects and sends nothing parks
 * that call until the shell's cancel handle fires or the shell is closed.
 *
 * <pre>{@code
 * try (Listener listener = Listener.open("tcp://0.0.0.0:0?listen=1");
 *      Receiver rx = listener.accept().intoReceiver()) {
 *     byte[] ts = rx.recvBytes();
 * }
 * }</pre>
 *
 * <p>Mirrors {@code tstrans.tcp.Transport} in tst-py.
 */
public final class Transport extends NativeHandle {

    static { NativeLoader.load(); }

    /** Package-private: constructed by {@link Listener#accept()} only. */
    Transport(long h) { setHandle(h); }

    /**
     * The peer's address as {@code ip:port}, an IPv6 address in brackets
     * ({@code [::1]:5000}). It is the TCP source address: behind a proxy, the proxy's.
     *
     * @throws IllegalStateException if the connection is closed
     */
    public String peerAddr() {
        ensureOpen("Transport is closed");
        return nPeerAddr(peekHandle());
    }

    /**
     * Consume the connection and return a {@link Receiver} over it: TS bytes in, packet
     * aligned. The {@code Transport} is unusable afterwards.
     *
     * <p>A {@code tcps://} handshake runs on the first {@code recvBytes()}, which throws
     * its failure (a peer that is not speaking TLS: {@code TcpException(BROKEN)}). A
     * silent peer parks that call until the receiver's cancel handle fires or the receiver
     * is closed. The receiver reports no {@code localPort()}.
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
     * demuxer options. Same consumption, handshake and cancel rules as
     * {@link #intoReceiver()}; a {@code tcps://} handshake runs on the first
     * {@code recvEvent()}.
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
     * {@link #intoDemuxReceiver()} with explicit demuxer options. A {@code null} config
     * is refused before the connection is consumed, so this {@code Transport} stays
     * usable; past that check the connection is consumed even if the call throws.
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
        int strict = demuxConfig.strictMode().ordinal();
        long pesCapPerPid = demuxConfig.pesCapPerPid();
        long pesCapTotal = demuxConfig.pesCapTotal();
        boolean cfiTolerance = demuxConfig.cfiTolerance();
        int av1Carriage = demuxConfig.av1Carriage().ordinal();
        long auCellCapPerPid = demuxConfig.auCellCapPerPid();
        boolean lenientPsi = demuxConfig.lenientPsiReassembly();
        long syncBufCap = demuxConfig.syncBufCap();
        boolean unwrapTimestamps = demuxConfig.unwrapTimestamps();
        long conn = consumeHandle(); // consumed: moved into the DemuxReceiver
        return ShellAdapters.demuxReceiver(nIntoDemuxReceiverWithConfig(conn, strict,
            pesCapPerPid, pesCapTotal, cfiTolerance, av1Carriage, auCellCapPerPid, lenientPsi,
            syncBufCap, unwrapTimestamps));
    }

    /**
     * Consume the connection and return a {@link Sender} over it: pre-muxed TS bytes
     * out. Same consumption and cancel rules as {@link #intoReceiver()}; a
     * {@code tcps://} handshake runs on the first send that reaches the socket.
     *
     * @return a sender owning the connection; it throws {@link org.tstrans.TcpException}
     * @throws IllegalStateException if the connection is closed or already consumed
     */
    public Sender intoSender() {
        ensureOpen("Transport is closed");
        long conn = consumeHandle(); // consumed: moved into the Sender
        return ShellAdapters.sender(nIntoSender(conn));
    }

    /** Close the connection. Idempotent; does nothing once the connection is consumed. */
    @Override public void close() { super.close(); }

    @Override protected void nativeClose(long h) { nClose(h); }

    private static native String nPeerAddr(long handle);

    private static native void nClose(long handle);

    private static native long nIntoReceiver(long handle);

    private static native long nIntoDemuxReceiver(long handle);

    private static native long nIntoDemuxReceiverWithConfig(long handle, int strict,
        long pesCapPerPid, long pesCapTotal, boolean cfiTolerance, int av1Carriage,
        long auCellCapPerPid, boolean lenientPsi, long syncBufCap, boolean unwrapTimestamps);

    private static native long nIntoSender(long handle);
}
