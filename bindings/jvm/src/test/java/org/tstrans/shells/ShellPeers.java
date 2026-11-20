package org.tstrans.shells;

import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.util.ArrayDeque;
import java.util.Deque;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.tstrans.BindingException;
import org.tstrans.RistException;
import org.tstrans.RtpException;
import org.tstrans.SrtException;
import org.tstrans.TcpException;
import org.tstrans.TestSupport;
import org.tstrans.UdpException;
import org.tstrans.mpegts.DemuxEvent;

/**
 * Live peers and per-shell operations shared by the shell alias and lifecycle
 * tests, so each helper is written once. Every parked call here has a watchdog
 * daemon thread holding a pre-obtained {@link CancelHandle}: a hung JUnit test
 * would wedge a gating runner.
 */
public final class ShellPeers {
    private ShellPeers() {}

    static final long PEER_WAIT_MS = 10_000;
    static final byte[] TS_PACKET = tsPacket();
    static final byte[] TS_BUNDLE = bundle();
    static final byte[] NAL_IDR = {0, 0, 0, 1, 0x65, (byte) 0xBB};
    /** More than the sender's 18 800-byte framing limit, and no 0x47 anywhere. */
    static final byte[] NOT_TS = new byte[20_000];

    private static byte[] tsPacket() {
        byte[] p = new byte[188];
        p[0] = 0x47;
        return p;
    }

    private static byte[] bundle() {
        byte[] b = new byte[188 * 6];
        for (int i = 0; i < 6; i++) System.arraycopy(TS_PACKET, 0, b, i * 188, 188);
        return b;
    }

    /** Closes what it was given, last in first out, ignoring failures. */
    public static final class Cleanup implements AutoCloseable {
        private final Deque<Runnable> undo = new ArrayDeque<>();

        public synchronized <T extends AutoCloseable> T add(T closeable) {
            undo.push(() -> {
                try {
                    closeable.close();
                } catch (Exception ignored) {
                    // best-effort teardown
                }
            });
            return closeable;
        }

        public synchronized void add(Runnable r) { undo.push(r); }

        @Override public synchronized void close() {
            while (!undo.isEmpty()) {
                try {
                    undo.pop().run();
                } catch (RuntimeException ignored) {
                    // best-effort teardown
                }
            }
        }
    }

    /** Result of an open that runs on a daemon thread because it blocks until a peer connects. */
    static final class Pending<T> {
        private final CompletableFuture<T> future = new CompletableFuture<>();

        T await() throws Exception {
            try {
                return future.get(PEER_WAIT_MS, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                throw new AssertionError("the listener did not accept within " + PEER_WAIT_MS + " ms", e);
            }
        }
    }

    static <T> Pending<T> openInBackground(Callable<T> open) {
        Pending<T> p = new Pending<>();
        Thread t = new Thread(() -> {
            try {
                p.future.complete(open.call());
            } catch (Throwable e) {
                p.future.completeExceptionally(e);
            }
        }, "shell-peer-open");
        t.setDaemon(true);
        t.start();
        return p;
    }

    /** Open a caller against a listener that may not be accepting yet. */
    static <T> T retryCaller(Callable<T> open) throws Exception {
        long end = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(PEER_WAIT_MS);
        while (true) {
            try {
                return open.call();
            } catch (SrtException e) {
                if (System.nanoTime() > end) throw e;
                Thread.sleep(50);
            }
        }
    }

    /** An opened thing and the even port librist bound it on. */
    record Bound<T>(T value, int port) {}

    /** Open on a free even loopback port, or skip the test when librist cannot bind one. */
    static <T> Bound<T> bindEven(java.util.function.IntFunction<Callable<T>> open) {
        for (int attempt = 0; attempt < 20; attempt++) {
            int port;
            try {
                port = TestSupport.freeUdpPort() & ~1;
            } catch (java.io.IOException e) {
                continue;
            }
            try {
                return new Bound<>(open.apply(port).call(), port);
            } catch (RistException e) {
                // librist could not bind that port (taken since the probe); try another
            } catch (Exception e) {
                throw new AssertionError("opening on even port " + port + " failed", e);
            }
        }
        assumeTrue(false, "librist could not bind an even loopback port");
        throw new IllegalStateException("unreachable");
    }

    /** The transport's exception class for a scheme. */
    static Class<? extends BindingException> errorClass(String scheme) {
        switch (scheme) {
            case "srt": return SrtException.class;
            case "rtp": return RtpException.class;
            case "udp": return UdpException.class;
            case "tcp": return TcpException.class;
            case "rist": return RistException.class;
            default: throw new IllegalArgumentException(scheme);
        }
    }

    /** {@code kind().name()} of any transport exception. */
    static String kindName(Throwable e) {
        if (e instanceof SrtException x) return x.kind().name();
        if (e instanceof RtpException x) return x.kind().name();
        if (e instanceof UdpException x) return x.kind().name();
        if (e instanceof TcpException x) return x.kind().name();
        if (e instanceof RistException x) return x.kind().name();
        throw new AssertionError("not a transport exception: " + e);
    }

    /** Run {@code call} on a daemon thread and fail unless it returns within {@code ms}. */
    public static <T> T within(long ms, String what, Callable<T> call) throws Exception {
        CompletableFuture<T> f = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try {
                f.complete(call.call());
            } catch (Throwable e) {
                f.completeExceptionally(e);
            }
        }, "shell-accessor");
        t.setDaemon(true);
        t.start();
        try {
            return f.get(ms, TimeUnit.MILLISECONDS);
        } catch (TimeoutException e) {
            throw new AssertionError(what + " did not answer within " + ms + " ms");
        }
    }

    // ----------------------------------------------------------------------------------------
    // Shell operations
    // ----------------------------------------------------------------------------------------

    /** A sender shell, whichever of Sender / MuxSender it is. */
    interface Tx extends AutoCloseable {
        void sendOnce() throws Exception;
        /** Raw TS bytes; only a {@code Sender} takes them. */
        void sendBytes(byte[] bytes) throws Exception;
        CancelHandle cancelHandle();
        boolean isAlive();
        @Override void close();
    }

    /** A receiver shell, whichever of Receiver / DemuxReceiver it is. */
    interface Rx extends AutoCloseable {
        /** One blocking receive; returns normally only when data arrived. */
        void recvOnce() throws Exception;
        CancelHandle cancelHandle();
        boolean isAlive();
        java.util.OptionalInt localPort();
        java.util.Optional<java.net.InetSocketAddress> localAddr();
        java.util.Optional<java.net.InetSocketAddress> peerAddr();
        long sourceRejected();
        SocketStats socketStats();
        java.util.Optional<EndReason> endReason();
        java.util.Optional<String> endDetail();
        @Override void close();
    }

    static Tx openTx(String shell, String url) throws Exception {
        if (shell.equals("Sender")) {
            Sender s = Sender.open(url);
            return new Tx() {
                @Override public void sendOnce() throws Exception { s.sendBytes(TS_BUNDLE); }
                @Override public void sendBytes(byte[] bytes) throws Exception { s.sendBytes(bytes); }
                @Override public CancelHandle cancelHandle() { return s.cancelHandle(); }
                @Override public boolean isAlive() { return s.isAlive(); }
                @Override public void close() { s.close(); }
            };
        }
        MuxSender m = MuxSender.open(url, TestSupport.roundtripConfig());
        return new Tx() {
            @Override public void sendOnce() throws Exception { m.sendVideo(NAL_IDR, 0L, true); }
            @Override public void sendBytes(byte[] bytes) {
                throw new UnsupportedOperationException("a MuxSender takes no raw TS bytes");
            }
            @Override public CancelHandle cancelHandle() { return m.cancelHandle(); }
            @Override public boolean isAlive() { return m.isAlive(); }
            @Override public void close() { m.close(); }
        };
    }

    static Rx openRx(String shell, String url) throws Exception {
        if (shell.equals("Receiver")) {
            Receiver r = Receiver.open(url);
            return new Rx() {
                @Override public void recvOnce() throws Exception { r.recvBytes(); }
                @Override public CancelHandle cancelHandle() { return r.cancelHandle(); }
                @Override public boolean isAlive() { return r.isAlive(); }
                @Override public java.util.OptionalInt localPort() { return r.localPort(); }
                @Override public java.util.Optional<java.net.InetSocketAddress> localAddr() { return r.localAddr(); }
                @Override public java.util.Optional<java.net.InetSocketAddress> peerAddr() { return r.peerAddr(); }
                @Override public long sourceRejected() { return r.sourceRejected(); }
                @Override public SocketStats socketStats() { return r.socketStats(); }
                @Override public java.util.Optional<EndReason> endReason() { return r.endReason(); }
                @Override public java.util.Optional<String> endDetail() { return r.endDetail(); }
                @Override public void close() { r.close(); }
            };
        }
        DemuxReceiver d = DemuxReceiver.open(url);
        return new Rx() {
            @Override public void recvOnce() throws Exception {
                DemuxEvent e = d.recvEvent();
                if (e == null) throw new AssertionError("the iterator ended instead of parking");
            }
            @Override public CancelHandle cancelHandle() { return d.cancelHandle(); }
            @Override public boolean isAlive() { return d.isAlive(); }
            @Override public java.util.OptionalInt localPort() { return d.localPort(); }
            @Override public java.util.Optional<java.net.InetSocketAddress> localAddr() { return d.localAddr(); }
            @Override public java.util.Optional<java.net.InetSocketAddress> peerAddr() { return d.peerAddr(); }
            @Override public long sourceRejected() { return d.sourceRejected(); }
            @Override public SocketStats socketStats() { return d.socketStats(); }
            @Override public java.util.Optional<EndReason> endReason() { return d.endReason(); }
            @Override public java.util.Optional<String> endDetail() { return d.endDetail(); }
            @Override public void close() { d.close(); }
        };
    }

    // ----------------------------------------------------------------------------------------
    // Live peers
    // ----------------------------------------------------------------------------------------

    /** An open receiver shell with a live, silent peer attached; everything is closed with {@code c}. */
    static Rx receiverWithPeer(String shell, String scheme, Cleanup c) throws Exception {
        return receiverAndPeer(shell, scheme, c).rx();
    }

    /**
     * A receiver shell with a live, silent peer, plus a runnable that closes the peer cleanly (a
     * no-op on the datagram transports, whose peer is not connected).
     */
    record ReceiverPeer(Rx rx, Runnable closePeer) {}

    static ReceiverPeer receiverAndPeer(String shell, String scheme, Cleanup c) throws Exception {
        int port = TestSupport.freeUdpPort();
        Rx rx;
        Runnable closePeer = () -> {};
        switch (scheme) {
            case "srt": {
                Pending<Rx> listening = openInBackground(
                    () -> openRx(shell, "srt://:" + port + "?mode=listener"));
                Sender peer = c.add(retryCaller(() -> Sender.open("srt://127.0.0.1:" + port + "?mode=caller")));
                closePeer = peer::close;
                rx = listening.await();
                break;
            }
            case "rtp":
            case "udp":
                rx = openRx(shell, scheme + "://127.0.0.1:" + port);
                break;
            case "tcp": {
                java.net.ServerSocket server = new java.net.ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress());
                c.add(server);
                CompletableFuture<java.net.Socket> accepted = new CompletableFuture<>();
                Thread accept = new Thread(() -> {
                    try {
                        accepted.complete(c.add(server.accept()));
                    } catch (java.io.IOException e) {
                        // closed during teardown
                        accepted.completeExceptionally(e);
                    }
                }, "shell-tcp-accept");
                accept.setDaemon(true);
                accept.start();
                rx = openRx(shell, "tcp://127.0.0.1:" + server.getLocalPort());
                closePeer = () -> {
                    try {
                        accepted.get(PEER_WAIT_MS, TimeUnit.MILLISECONDS).close();
                    } catch (Exception e) {
                        throw new AssertionError("the tcp peer was not accepted", e);
                    }
                };
                break;
            }
            case "rist":
                rx = bindEven(p -> () -> openRx(shell, "rist://@127.0.0.1:" + p)).value();
                break;
            default:
                throw new IllegalArgumentException(scheme);
        }
        c.add(rx);
        return new ReceiverPeer(rx, closePeer);
    }

    /**
     * A UDP socket on a second source IP: {@code socket} sends to {@code dest}, which reaches a
     * receiver bound to {@code receiverHost} (URL host form) on the same port.
     */
    public record SecondSource(java.net.DatagramSocket socket, java.net.InetAddress dest,
                               String receiverHost) {}

    /**
     * A socket on 127.0.0.2 where that binds (Linux routes all of 127/8 to loopback), with the
     * receiver on 127.0.0.1; otherwise on ::1 with a dual-stack {@code [::]} receiver. On Linux
     * neither working is a failure; elsewhere the test is skipped.
     */
    public static SecondSource secondSource(Cleanup c) throws Exception {
        java.net.InetAddress v4 = java.net.InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        String why;
        try {
            java.net.InetAddress second = java.net.InetAddress.getByAddress(new byte[] {127, 0, 0, 2});
            java.net.DatagramSocket b = c.add(new java.net.DatagramSocket(new java.net.InetSocketAddress(second, 0)));
            return new SecondSource(b, v4, "127.0.0.1");
        } catch (java.io.IOException e) {
            why = "127.0.0.2: " + e;
        }
        try {
            java.net.InetAddress v6 = java.net.InetAddress.getByAddress(
                new byte[] {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1});
            java.net.DatagramSocket b = c.add(new java.net.DatagramSocket(new java.net.InetSocketAddress(v6, 0)));
            return new SecondSource(b, v6, "[::]");
        } catch (java.io.IOException e) {
            why += "; ::1: " + e;
        }
        if (TestSupport.isLinux()) throw new AssertionError("no second UDP source address: " + why);
        System.out.println("skipping: no second UDP source address: " + why);
        assumeTrue(false, "no second UDP source address: " + why);
        throw new IllegalStateException("unreachable");
    }

    /**
     * Two UDP sockets with different source IPs that both reach a receiver bound to
     * {@code receiverHost}: {@code a} sends from 127.0.0.1 to {@code aDest}, {@code b} is
     * {@link #secondSource}'s socket.
     */
    public record TwoSources(java.net.DatagramSocket a, java.net.InetAddress aDest,
                             java.net.DatagramSocket b, java.net.InetAddress bDest,
                             String receiverHost) {}

    /** {@link #secondSource} plus a first socket on 127.0.0.1; the same skip rule. */
    public static TwoSources twoUdpSources(Cleanup c) throws Exception {
        java.net.InetAddress v4 = java.net.InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        java.net.DatagramSocket a = c.add(new java.net.DatagramSocket(new java.net.InetSocketAddress(v4, 0)));
        SecondSource b = secondSource(c);
        return new TwoSources(a, v4, b.socket(), b.dest(), b.receiverHost());
    }

    /**
     * A two-way UDP relay on a daemon thread, for a sender that cannot choose its source
     * address: it dials {@link #frontPort()} on 127.0.0.1, and the relay forwards each datagram
     * to {@code to} out of {@code outward}, so the receiver sees {@code outward}'s address, and
     * sends what comes back on {@code outward} to the sender's last address. Closing stops it.
     */
    public static final class UdpRelay implements AutoCloseable {
        private final java.net.DatagramSocket front;
        private final java.util.concurrent.atomic.AtomicBoolean stop =
            new java.util.concurrent.atomic.AtomicBoolean();
        private final java.util.concurrent.atomic.AtomicLong forwarded =
            new java.util.concurrent.atomic.AtomicLong();
        private final Thread thread;

        public UdpRelay(java.net.DatagramSocket outward, java.net.InetSocketAddress to)
                throws java.io.IOException {
            front = new java.net.DatagramSocket(new java.net.InetSocketAddress(
                java.net.InetAddress.getByAddress(new byte[] {127, 0, 0, 1}), 0));
            front.setSoTimeout(2);
            outward.setSoTimeout(1);
            thread = new Thread(() -> run(outward, to), "udp-relay");
            thread.setDaemon(true);
            thread.start();
        }

        private void run(java.net.DatagramSocket outward, java.net.InetSocketAddress to) {
            byte[] buf = new byte[65536];
            java.net.SocketAddress sender = null;
            while (!stop.get()) {
                try {
                    java.net.DatagramPacket in = new java.net.DatagramPacket(buf, buf.length);
                    try {
                        front.receive(in);
                        sender = in.getSocketAddress();
                        outward.send(new java.net.DatagramPacket(buf, in.getLength(), to));
                        forwarded.incrementAndGet();
                    } catch (java.net.SocketTimeoutException idle) {
                        // nothing from the sender this tick
                    }
                    while (true) {
                        java.net.DatagramPacket back = new java.net.DatagramPacket(buf, buf.length);
                        try {
                            outward.receive(back);
                        } catch (java.net.SocketTimeoutException idle) {
                            break;
                        }
                        if (sender != null) {
                            front.send(new java.net.DatagramPacket(buf, back.getLength(), sender));
                        }
                    }
                } catch (java.io.IOException e) {
                    // a socket was closed under the relay at teardown, or a send failed
                    if (front.isClosed() || outward.isClosed()) return;
                }
            }
        }

        public int frontPort() { return front.getLocalPort(); }

        /** Datagrams relayed towards the receiver so far. */
        public long forwarded() { return forwarded.get(); }

        @Override public void close() {
            stop.set(true);
            try {
                thread.join(5_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            front.close();
        }
    }

    /** One packet from {@code rx}, retrying librist's BACKPRESSURE tick while nothing has arrived. */
    public static byte[] recvTolerant(Receiver rx) throws Exception {
        while (true) {
            try {
                return rx.recvBytes();
            } catch (RistException e) {
                if (e.kind() != RistException.Kind.BACKPRESSURE) throw e;
            }
        }
    }

    /** A sender shell with a live peer draining it, plus a runnable that makes the peer go away. */
    record SenderPeer(Tx tx, Runnable dropPeer) {}

    static SenderPeer senderWithPeer(String shell, String scheme, Cleanup c) throws Exception {
        int port = TestSupport.freeUdpPort();
        switch (scheme) {
            case "srt": {
                Pending<Receiver> listening = openInBackground(
                    () -> Receiver.open("srt://:" + port + "?mode=listener"));
                Tx tx = c.add(retryCaller(() -> openTx(shell, "srt://127.0.0.1:" + port + "?mode=caller")));
                Receiver peer = c.add(listening.await());
                return new SenderPeer(tx, peer::close);
            }
            case "rtp":
            case "udp": {
                java.net.DatagramSocket sink = c.add(new java.net.DatagramSocket(0, java.net.InetAddress.getLoopbackAddress()));
                Tx tx = c.add(openTx(shell, scheme + "://127.0.0.1:" + sink.getLocalPort()));
                return new SenderPeer(tx, sink::close);
            }
            case "tcp": {
                java.net.ServerSocket server = new java.net.ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress());
                c.add(server);
                java.util.concurrent.atomic.AtomicReference<java.net.Socket> conn = new java.util.concurrent.atomic.AtomicReference<>();
                CountDownLatch accepted = new CountDownLatch(1);
                Thread drain = new Thread(() -> {
                    try (java.net.Socket s = server.accept()) {
                        conn.set(s);
                        accepted.countDown();
                        byte[] buf = new byte[65536];
                        while (s.getInputStream().read(buf) >= 0) {
                            // drain until the peer is dropped
                        }
                    } catch (java.io.IOException ignored) {
                        // the peer was dropped
                    }
                }, "shell-tcp-drain");
                drain.setDaemon(true);
                drain.start();
                Tx tx = c.add(openTx(shell, "tcp://127.0.0.1:" + server.getLocalPort()));
                return new SenderPeer(tx, () -> {
                    try {
                        accepted.await(PEER_WAIT_MS, TimeUnit.MILLISECONDS);
                        java.net.Socket s = conn.get();
                        if (s != null) s.close();
                        server.close();
                    } catch (Exception ignored) {
                        // already gone
                    }
                });
            }
            case "rist": {
                Bound<Receiver> peer = bindEven(p -> () -> Receiver.open("rist://@127.0.0.1:" + p));
                c.add(peer.value());
                Tx tx = c.add(openTx(shell, "rist://127.0.0.1:" + peer.port()));
                return new SenderPeer(tx, peer.value()::close);
            }
            default:
                throw new IllegalArgumentException(scheme);
        }
    }

    // ----------------------------------------------------------------------------------------
    // Parked receives
    // ----------------------------------------------------------------------------------------

    /**
     * Cancels through a handle taken before the call under test if that call is still running
     * after {@code ms}: a hung call on the main thread would wedge the runner.
     */
    public static final class Watchdog implements AutoCloseable {
        private final CountDownLatch finished = new CountDownLatch(1);

        /**
         * Takes ownership of {@code handle}: the watchdog thread closes it once it can no
         * longer cancel (after its wait, whether it cancelled or not), so pass a fresh handle
         * and do not use it afterwards.
         */
        public Watchdog(CancelHandle handle, long ms) {
            Thread t = new Thread(() -> {
                try {
                    if (!finished.await(ms, TimeUnit.MILLISECONDS)) handle.cancel();
                } catch (InterruptedException ignored) {
                    // the test finished first
                } finally {
                    handle.close();
                }
            }, "shell-call-watchdog");
            t.setDaemon(true);
            t.start();
        }

        @Override public void close() { finished.countDown(); }
    }

    /** A receive parked on a daemon thread, with a watchdog that cancels it if the test hangs. */
    public static final class Parked implements AutoCloseable {
        final CompletableFuture<Throwable> outcome = new CompletableFuture<>();
        private final CountDownLatch finished = new CountDownLatch(1);

        Parked(Rx rx) throws InterruptedException {
            this(rx.cancelHandle(), "a silent peer delivered data", () -> {
                while (true) {
                    try {
                        rx.recvOnce();
                    } catch (RistException e) {
                        // librist's receive ticks BACKPRESSURE every 100 ms while no
                        // packet arrives; the receive is still waiting.
                        if (e.kind() != RistException.Kind.BACKPRESSURE) throw e;
                        continue;
                    }
                    return null;
                }
            });
        }

        /**
         * {@code call} parked on a daemon thread; {@code watchdogHandle} must cancel it. A
         * normal return is recorded as a failure named {@code returned}.
         */
        public Parked(CancelHandle watchdogHandle, String returned, Callable<?> call)
                throws InterruptedException {
            Thread watchdog = new Thread(() -> {
                try {
                    if (!finished.await(30, TimeUnit.SECONDS)) watchdogHandle.cancel();
                } catch (InterruptedException ignored) {
                    // the test finished first
                }
            }, "shell-watchdog");
            watchdog.setDaemon(true);
            watchdog.start();
            Thread t = new Thread(() -> {
                try {
                    call.call();
                    outcome.complete(new AssertionError(returned));
                } catch (Throwable e) {
                    outcome.complete(e);
                }
            }, "shell-parked-call");
            t.setDaemon(true);
            t.start();
            Thread.sleep(300); // let the receive park
        }

        /** The exception that ended the receive, within {@code ms}. */
        public Throwable awaitEnd(long ms) throws Exception {
            try {
                return outcome.get(ms, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                throw new AssertionError("the parked receive did not end within " + ms + " ms");
            }
        }

        @Override public void close() { finished.countDown(); }
    }
}
