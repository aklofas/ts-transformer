package org.tstrans.shells;

import java.net.InetSocketAddress;
import java.util.Optional;
import java.util.OptionalInt;
import org.tstrans.BindingException;
import org.tstrans.NativeHandle;
import org.tstrans.NativeLoader;
import org.tstrans.internal.EndReasonSnapshot;
import org.tstrans.internal.NativeAddr;
import org.tstrans.rtp.RtpStats;
import org.tstrans.srt.SrtStats;

/**
 * Any transport in, TS bytes out. Opened from any transport's URL
 * ({@code srt://}, {@code rtp://}, {@code udp://}, {@code tcp://}, {@code tcps://},
 * {@code rist://}); a transport failure throws that transport's exception
 * ({@code SrtException}, {@code RtpException}, {@code UdpException},
 * {@code TcpException}, {@code RistException}), each a {@link BindingException}.
 *
 * <p>An {@code srt://} URL must carry {@code mode=listener}: the open binds,
 * listens and accepts ONE peer, and blocks until one connects. No
 * {@link #cancelHandle()} exists until the open returns; to end that first
 * accept from another thread, pass an {@link OpenCancel} to
 * {@link #open(String, OpenCancel)} and cancel it. For a server that handles
 * many peers, use {@code org.tstrans.srt.Listener}.
 *
 * <p><b>Thread safety:</b> a single {@code Receiver} is NOT thread-safe. Use one
 * receiver per thread, or guard with external synchronisation.
 *
 * <p><b>Closing:</b> use try-with-resources or call {@link #close()} explicitly.
 * After close, further calls throw {@code IllegalStateException}.
 *
 * <p><b>Byte-copy posture (JDK 17):</b> {@link #recvBytes()} returns a heap
 * {@code byte[]} copy of the received packet; a zero-copy path (FFM
 * {@code MemorySegment}) is JDK-22+ only.
 *
 * <p><b>Receive quantum:</b> each {@link #recvBytes()} call returns one 188-byte
 * TS packet.
 *
 * <p>Mirrors {@code tstrans.shells.Receiver} in tst-py.
 */
public class Receiver extends NativeHandle {
    static { NativeLoader.load(); }

    // The end reason as it stood when close() ran; the native handle is gone by
    // then. Written once by nativeClose, read by endReason()/endDetail().
    private volatile EndReason closedEndReason;
    private volatile String closedEndDetail;
    private final RtpCounters rtp;

    /** Wrap a native handle returned by {@link #openHandle(String)}. */
    protected Receiver(long handle) {
        setHandle(handle);
        rtp = new RtpCounters(nRtpStats(handle));
    }

    /**
     * Open a receiver on the transport named by the URL's scheme.
     *
     * @param url {@code scheme://...} with per-transport query options
     * @return an open {@code Receiver}
     * @throws IllegalArgumentException if the scheme is not one of the six above
     * @throws BindingException the transport's exception for a malformed URL or a
     *     bind/connect fault. An {@code srt://} bind or accept fault is
     *     {@code SrtException(BROKEN)} with the message prefixed {@code bind: }
     *     or {@code accept: }.
     */
    public static Receiver open(String url) throws BindingException {
        return new Receiver(openHandle(url));
    }

    /**
     * Same as {@link #open(String)}, under {@code cancel}: its
     * {@link OpenCancel#cancel()} from another thread ends an {@code srt://}
     * first accept, and an object already cancelled refuses the open before the
     * URL is parsed. Either way the open throws the URL's transport exception
     * with kind {@code CLOSED} (see {@link OpenCancel} for every rule).
     *
     * @param url    {@code scheme://...} with per-transport query options
     * @param cancel the open's cancel; {@code null} is the same as {@link #open(String)}
     * @return an open {@code Receiver}
     * @throws IllegalArgumentException if the scheme is not one of the six above
     * @throws IllegalStateException if {@code cancel} is closed
     * @throws BindingException as in {@link #open(String)}; kind {@code CLOSED}
     *     when {@code cancel} was cancelled by the time the open finished
     */
    public static Receiver open(String url, OpenCancel cancel) throws BindingException {
        return new Receiver(nOpen(url, cancel));
    }

    /**
     * Open the native receiver and return its handle, for a subclass's own
     * factory. Same contract as {@link #open(String)}.
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
     * Receive one TS packet. Blocks until a packet is available.
     *
     * <p>After {@code cancelHandle().cancel()} this throws the transport's
     * {@code CLOSED}, buffered data included.
     *
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws BindingException the transport's exception: {@code END_OF_STREAM}
     *     when the peer closed cleanly ({@code srt://} reads {@code BROKEN} first,
     *     because libsrt reports a broken link rather than a clean EOF),
     *     {@code BROKEN} if the transport is dead, {@code CLOSED} if cancelled
     */
    public byte[] recvBytes() throws BindingException {
        ensureOpen("Receiver is closed");
        return nRecvBytes(peekHandle());
    }

    /**
     * {@link #recvBytes()}: {@code maxLen} is ignored, one 188-byte packet per call.
     *
     * @param maxLen ignored
     * @return a 188-byte TS packet
     * @throws IllegalStateException if the receiver is closed
     * @throws BindingException the transport's exception
     * @deprecated the argument does nothing; call {@link #recvBytes()}. Removed in 0.12.0.
     */
    @Deprecated(since = "0.11.0", forRemoval = true)
    public byte[] recvBytes(int maxLen) throws BindingException {
        ensureOpen("Receiver is closed");
        return nRecvBytes(peekHandle());
    }

    /**
     * Return a shareable cancel handle. Calling {@link CancelHandle#cancel()}
     * on any handle wakes a thread parked in {@link #recvBytes()}; that call
     * throws the transport's exception with kind {@code CLOSED}.
     *
     * @return a new {@link CancelHandle}
     * @throws IllegalStateException if the receiver is closed
     */
    public CancelHandle cancelHandle() {
        ensureOpen("Receiver is closed");
        return new CancelHandle(nCancelHandle(peekHandle()));
    }

    /**
     * Scheme-neutral 16-field wire stats snapshot, the same on every transport.
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
     * @return the wire-stats snapshot
     * @throws IllegalStateException if the receiver is closed
     */
    public SocketStats socketStats() {
        ensureOpen("Receiver is closed");
        return nSocketStats(peekHandle());
    }

    /**
     * The receive counters: TS bytes and packets the receiver returned, bytes it
     * skipped to find the packet boundary, and how many times it had to find it. The
     * wire's own counters are {@link #socketStats()}.
     *
     * <p>Waits while another thread is parked in {@link #recvBytes()}: the counters
     * live in the receiver, which a parked receive holds until it returns.
     *
     * @return the counters as they stand now
     * @throws IllegalStateException if the receiver is closed
     */
    public ReceiverStats stats() {
        ensureOpen("Receiver is closed");
        return nStats(peekHandle());
    }

    /**
     * The RTP counters of a receiver over {@code rtp://}: datagrams dropped as
     * malformed, and those the URL's {@code source} key turned away. Empty on every
     * other transport. It never waits behind a {@link #recvBytes()} parked on another
     * thread, a read never returns less than an earlier one, and after
     * {@link #close()} it returns the counters as the close left them.
     *
     * @return the RTP counters, or empty off RTP
     */
    public Optional<RtpStats> rtpStats() {
        long h = peekHandle();
        return rtp.read(h == 0 ? null : nRtpStats(h));
    }

    /**
     * SRT-specific stats snapshot.
     *
     * @return the SRT stats
     * @throws IllegalStateException if the receiver is closed
     * @throws UnsupportedOperationException unless the receiver was opened from
     *     an {@code srt://} URL
     * @throws BindingException {@code SrtException} if the stats call fails
     */
    public SrtStats srtStats() throws BindingException {
        ensureOpen("Receiver is closed");
        return nSrtStats(peekHandle());
    }

    /**
     * Why the receive session ended; empty while it has not ended.
     * Every plain receive transport ({@code rtp}, {@code srt}, {@code udp},
     * {@code tcp}, {@code rist}) records one; {@code srt} never records a
     * clean teardown, because libsrt reports a peer's close as a broken link.
     * Lock-free, so it answers while
     * {@link #recvBytes()} is parked on another thread; once the receiver is
     * closed it returns the reason as it stood after close.
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
     * Close the receiver. Cancels first: a {@link #recvBytes()} parked on another
     * thread ends promptly with the transport's {@code CLOSED}, and this call
     * returns without waiting for data. Idempotent. After close, further
     * {@link #recvBytes} calls throw {@code IllegalStateException}.
     */
    @Override public void close() { super.close(); }

    /**
     * The local port the transport bound when this receiver was opened, for a
     * {@code udp://} or {@code rtp://} URL or a {@code rist://@host:port} listener, so
     * a URL with port 0 reports the port the kernel picked. For an {@code srt://}
     * listener it is the port the accepted connection arrived on: the open returns
     * only once a caller connected, so port 0 in an {@code srt://} URL cannot tell a
     * caller where to dial. Empty for every other shell (a caller, a connection
     * accepted by a listener object, a receiver taken from an RTSP session or a
     * publish mount). The value is the one captured at open; it never waits behind a
     * {@code recvBytes()} parked on another thread. A RIST receiver on port 0 needs the Main profile (the
     * default); the Simple profile is refused at open with kind
     * {@code INVALID_CONFIG}, because its second port cannot follow a
     * kernel-picked one.
     *
     * @return the bound port, or empty when the transport reports none
     * @throws IllegalStateException if the receiver is closed
     */
    public OptionalInt localPort() {
        ensureOpen("Receiver is closed");
        int p = nLocalPort(peekHandle());
        return p < 0 ? OptionalInt.empty() : OptionalInt.of(p);
    }

    /**
     * The address the transport bound when this receiver was opened: present exactly
     * where {@link #localPort()} is, with the same port. The value is the one captured
     * at open; it never waits behind a {@code recvBytes()} parked on another thread.
     *
     * @return the bound address, or empty when the transport reports none
     * @throws IllegalStateException if the receiver is closed
     */
    public Optional<InetSocketAddress> localAddr() {
        ensureOpen("Receiver is closed");
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
     * {@code recvBytes()} parked on another thread.
     *
     * @return the sender's address, or empty
     * @throws IllegalStateException if the receiver is closed
     */
    public Optional<InetSocketAddress> peerAddr() {
        ensureOpen("Receiver is closed");
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
     * waits behind a {@code recvBytes()} parked on another thread.
     *
     * @return the count of datagrams or blocks turned away
     * @throws IllegalStateException if the receiver is closed
     */
    public long sourceRejected() {
        ensureOpen("Receiver is closed");
        return nSourceRejected(peekHandle());
    }

    /**
     * Return {@code true} while the receiver owns a live transport. A receive in
     * flight on another thread counts as live.
     *
     * @return liveness state of the underlying transport
     */
    public boolean isAlive() {
        long h = peekHandle();
        return h != 0 && nIsAlive(h);
    }

    @Override protected void nativeClose(long h) {
        long[] rtpOut = {-1, -1};
        EndReasonSnapshot snapshot = nClose(h, rtpOut);
        rtp.atClose(rtpOut);
        if (snapshot != null) {
            closedEndDetail = snapshot.detail();
            closedEndReason = snapshot.reason();
        }
    }

    private static native long    nOpen(String url, OpenCancel cancel) throws BindingException;
    private static native byte[]  nRecvBytes(long handle) throws BindingException;
    private static native long    nCancelHandle(long handle);
    private static native SocketStats nSocketStats(long handle);
    private static native ReceiverStats nStats(long handle);
    private static native RtpStats    nRtpStats(long handle);
    private static native SrtStats    nSrtStats(long handle) throws BindingException;
    private static native int     nEndReason(long handle);
    private static native String  nEndDetail(long handle);
    private static native EndReasonSnapshot nClose(long handle, long[] rtpOut);
    private static native int nLocalPort(long handle);
    private static native byte[] nLocalAddr(long handle);
    private static native byte[] nPeerAddr(long handle);
    private static native long nSourceRejected(long handle);
    private static native boolean nIsAlive(long handle);
}
