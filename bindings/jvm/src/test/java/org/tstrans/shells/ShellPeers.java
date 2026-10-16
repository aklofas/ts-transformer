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
final class ShellPeers {
    private ShellPeers() {}

    static final long PEER_WAIT_MS = 10_000;
    static final byte[] TS_PACKET = tsPacket();
    static final byte[] TS_BUNDLE = bundle();
    static final byte[] NAL_IDR = {0, 0, 0, 1, 0x65, (byte) 0xBB};

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
    static final class Cleanup implements AutoCloseable {
        private final Deque<Runnable> undo = new ArrayDeque<>();

        synchronized <T extends AutoCloseable> T add(T closeable) {
            undo.push(() -> {
                try {
                    closeable.close();
                } catch (Exception ignored) {
                    // best-effort teardown
                }
            });
            return closeable;
        }

        synchronized void add(Runnable r) { undo.push(r); }

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
            try {
                int port = TestSupport.freeUdpPort() & ~1;
                return new Bound<>(open.apply(port).call(), port);
            } catch (Exception e) {
                // that port was taken or odd-only; try another
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
    static <T> T within(long ms, String what, Callable<T> call) throws Exception {
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
        SocketStats socketStats();
        EndReason endReason();
        @Override void close();
    }

    static Tx openTx(String shell, String url) throws Exception {
        if (shell.equals("Sender")) {
            Sender s = Sender.open(url);
            return new Tx() {
                @Override public void sendOnce() throws Exception { s.sendBytes(TS_BUNDLE); }
                @Override public CancelHandle cancelHandle() { return s.cancelHandle(); }
                @Override public boolean isAlive() { return s.isAlive(); }
                @Override public void close() { s.close(); }
            };
        }
        MuxSender m = MuxSender.open(url, TestSupport.roundtripConfig());
        return new Tx() {
            @Override public void sendOnce() throws Exception { m.sendVideo(NAL_IDR, 0L, true); }
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
                @Override public SocketStats socketStats() { return r.socketStats(); }
                @Override public EndReason endReason() { return r.endReason(); }
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
            @Override public SocketStats socketStats() { return d.socketStats(); }
            @Override public EndReason endReason() { return d.endReason(); }
            @Override public void close() { d.close(); }
        };
    }

    // ----------------------------------------------------------------------------------------
    // Live peers
    // ----------------------------------------------------------------------------------------

    /** An open receiver shell with a live, silent peer attached; everything is closed with {@code c}. */
    static Rx receiverWithPeer(String shell, String scheme, Cleanup c) throws Exception {
        int port = TestSupport.freeUdpPort();
        Rx rx;
        switch (scheme) {
            case "srt": {
                Pending<Rx> listening = openInBackground(
                    () -> openRx(shell, "srt://:" + port + "?mode=listener"));
                c.add(retryCaller(() -> Sender.open("srt://127.0.0.1:" + port + "?mode=caller")));
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
                Thread accept = new Thread(() -> {
                    try {
                        c.add(server.accept());
                    } catch (java.io.IOException ignored) {
                        // closed during teardown
                    }
                }, "shell-tcp-accept");
                accept.setDaemon(true);
                accept.start();
                rx = openRx(shell, "tcp://127.0.0.1:" + server.getLocalPort());
                break;
            }
            case "rist":
                rx = bindEven(p -> () -> openRx(shell, "rist://@127.0.0.1:" + p)).value();
                break;
            default:
                throw new IllegalArgumentException(scheme);
        }
        c.add(rx);
        return rx;
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

    /** A receive parked on a daemon thread, with a watchdog that cancels it if the test hangs. */
    static final class Parked implements AutoCloseable {
        final CompletableFuture<Throwable> outcome = new CompletableFuture<>();
        private final CountDownLatch finished = new CountDownLatch(1);

        Parked(Rx rx) throws InterruptedException {
            CancelHandle watchdogHandle = rx.cancelHandle();
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
                    while (true) {
                        try {
                            rx.recvOnce();
                        } catch (RistException e) {
                            // librist's receive ticks BACKPRESSURE every 100 ms while no
                            // packet arrives; the receive is still waiting.
                            if (e.kind() != RistException.Kind.BACKPRESSURE) throw e;
                            continue;
                        }
                        outcome.complete(new AssertionError("a silent peer delivered data"));
                        return;
                    }
                } catch (Throwable e) {
                    outcome.complete(e);
                }
            }, "shell-parked-recv");
            t.setDaemon(true);
            t.start();
            Thread.sleep(300); // let the receive park
        }

        /** The exception that ended the receive, within {@code ms}. */
        Throwable awaitEnd(long ms) throws Exception {
            try {
                return outcome.get(ms, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                throw new AssertionError("the parked receive did not end within " + ms + " ms");
            }
        }

        @Override public void close() { finished.countDown(); }
    }
}
