package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.scaledMs;

import java.lang.reflect.Modifier;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import java.net.SocketAddress;
import java.net.SocketTimeoutException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.function.BooleanSupplier;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import org.tstrans.BindingException;
import org.tstrans.SrtException;
import org.tstrans.internal.CancelProbe;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.srt.Builder;
import org.tstrans.srt.Listener;
import org.tstrans.srt.ManagedDemuxReceiver;
import org.tstrans.srt.ManagedMuxSender;
import org.tstrans.srt.ManagedReceiver;
import org.tstrans.srt.ManagedSender;
import org.tstrans.srt.ReconnectPolicy;

/**
 * {@link OpenCancel}: the host's cancel for an open that has not returned a shell yet.
 *
 * <p>A listener-mode {@code srt://} open blocks in its first accept before any
 * {@code cancelHandle()} exists. An {@code OpenCancel} made beforehand and passed to the
 * open ends that accept from another thread; an object already cancelled refuses every
 * open before it parses its URL; an open that finishes after the cancel throws
 * {@code CLOSED} whatever it did, so nothing is handed out. An open that returned before
 * the cancel is untouched by it.
 *
 * <p>Every open that can park runs on a daemon thread; a test that fails to wake it
 * connects a peer in {@code finally} so no thread is left in a native accept.
 */
class OpenCancelTest {
    /** Far longer than any healthy wake takes: a test fails, it does not hang. */
    private static final long DEADLINE_MS = scaledMs(10_000);

    // ----------------------------------------------------------------------------------------
    // Helpers
    // ----------------------------------------------------------------------------------------

    private static void waitFor(String what, BooleanSupplier cond) throws InterruptedException {
        long end = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(DEADLINE_MS);
        while (!cond.getAsBoolean()) {
            if (System.nanoTime() > end) fail("timed out after " + DEADLINE_MS + " ms waiting for " + what);
            Thread.sleep(10);
        }
    }

    /** {@code true} while something holds the loopback UDP {@code port} (a plain bind of it fails). */
    private static boolean udpPortBound(int port) {
        try (DatagramSocket s = new DatagramSocket(new InetSocketAddress("127.0.0.1", port))) {
            return false;
        } catch (java.io.IOException e) {
            return true;
        }
    }

    private static void assertClosed(Throwable e, Class<? extends BindingException> cls) {
        assertInstanceOf(cls, e, "expected " + cls.getSimpleName() + "(CLOSED), got " + e);
        assertEquals("CLOSED", ShellPeers.kindName(e), "expected CLOSED, got " + e);
    }

    private static void assertSrtClosed(Throwable e) { assertClosed(e, SrtException.class); }

    /** One open on a daemon thread, keeping what it returned or threw. */
    private static final class Opening {
        final CompletableFuture<Object> result = new CompletableFuture<>();
        final Thread thread;

        Opening(Callable<?> open) {
            thread = new Thread(() -> {
                try {
                    result.complete(open.call());
                } catch (Throwable e) {
                    result.completeExceptionally(e);
                }
            }, "open-cancel-open");
            thread.setDaemon(true);
            thread.start();
        }

        /** Join with the deadline; fail if the open is still running. */
        void join(String what) throws InterruptedException {
            thread.join(DEADLINE_MS);
            assertFalse(thread.isAlive(), what + ": the open is still parked");
        }

        /** What the open threw, or {@code null} after a normal return. */
        Throwable error() {
            if (!result.isCompletedExceptionally()) return null;
            try {
                result.join();
                return null;
            } catch (java.util.concurrent.CompletionException e) {
                return e.getCause();
            }
        }

        Object value() { return result.isCompletedExceptionally() ? null : result.getNow(null); }

        void closeResult() throws Exception {
            if (result.isDone() && value() instanceof AutoCloseable c) c.close();
        }
    }

    /**
     * Connect a peer to the listener on {@code port} when {@code o} is still parked, so a
     * cancel under test that failed to wake it leaves no thread in a native accept; then
     * close what the open returned.
     */
    private static void rescue(int port, Opening o) throws Exception {
        if (o.thread.isAlive()) {
            try (Sender tx = Sender.open("srt://127.0.0.1:" + port + "?conntimeo=3000")) {
                o.thread.join(DEADLINE_MS);
            } catch (SrtException ignored) {
                // the listener went away meanwhile
            }
        }
        o.thread.join(DEADLINE_MS);
        o.closeResult();
    }

    @FunctionalInterface
    private interface Open {
        Object open(String url, OpenCancel cancel) throws Exception;
    }

    /** The four opens that block in a listener-mode first accept. */
    private static Open listenerOpen(String name) {
        switch (name) {
            case "Receiver.open": return Receiver::open;
            case "DemuxReceiver.open":
                return (u, oc) -> DemuxReceiver.open(u, DemuxerConfig.builder().build(), oc);
            case "ManagedReceiver.fromUrl": return (u, oc) -> ManagedReceiver.fromUrl(u, null, oc);
            case "ManagedDemuxReceiver.fromUrl":
                return (u, oc) -> ManagedDemuxReceiver.fromUrl(u, DemuxerConfig.builder().build(), null, oc);
            default: throw new IllegalArgumentException(name);
        }
    }

    /** Every open that dials; the URL is a caller-mode URL. */
    private static Open callerOpen(String name) {
        switch (name) {
            case "Sender.open": return Sender::open;
            case "MuxSender.open": return (u, oc) -> MuxSender.open(u, roundtripConfig(), oc);
            case "ManagedSender.fromUrl": return (u, oc) -> ManagedSender.fromUrl(u, null, oc);
            case "ManagedMuxSender.fromUrl":
                return (u, oc) -> ManagedMuxSender.fromUrl(u, roundtripConfig(), null, oc);
            case "ManagedDemuxReceiver.fromUrl": return listenerOpen(name);
            default: throw new IllegalArgumentException(name);
        }
    }

    private record ParkedOpen(int port, Opening opening) {}

    /**
     * Start a listener-mode open with {@code oc}; return once its port is bound and it has not
     * returned. The open's bind fails with a {@code bind:} fault, before it parks, when
     * something holds the port at that moment: another socket that took it after
     * {@code freeUdpPort()} released it, or this loop's own {@code udpPortBound} probe, which
     * binds the same port every 10 ms while the open is binding. Such a port is given up for
     * another one, at most four times.
     */
    private static ParkedOpen park(Open open, OpenCancel oc) throws Exception {
        for (int attempt = 1; ; attempt++) {
            int port = freeUdpPort();
            String url = "srt://127.0.0.1:" + port + "?mode=listener";
            Opening o = new Opening(() -> open.open(url, oc));
            waitFor("the open to bind its port", () -> udpPortBound(port) || o.result.isDone());
            if (o.result.isDone() && attempt < 5 && o.error() instanceof SrtException e
                    && e.getMessage().startsWith("bind:")) {
                continue;
            }
            assertFalse(o.result.isDone(), () -> "the open returned instead of parking: " + o.error());
            return new ParkedOpen(port, o);
        }
    }

    // ----------------------------------------------------------------------------------------
    // cancel() ends an open parked in its first accept
    // ----------------------------------------------------------------------------------------

    @ParameterizedTest
    @ValueSource(strings = {"Receiver.open", "DemuxReceiver.open", "ManagedReceiver.fromUrl",
                            "ManagedDemuxReceiver.fromUrl"})
    @Timeout(60)
    void cancelEndsAParkedFirstAccept(String name) throws Exception {
        try (OpenCancel oc = new OpenCancel()) {
            ParkedOpen p = park(listenerOpen(name), oc);
            try {
                oc.cancel();
                p.opening().join(name);
                assertSrtClosed(p.opening().error());
                assertNull(p.opening().value());
                assertTrue(oc.isCancelled());
                waitFor("the port to be released", () -> !udpPortBound(p.port()));
            } finally {
                rescue(p.port(), p.opening());
            }
        }
    }

    @Test
    @Timeout(60)
    void oneCancelEndsTwoOpensParkedWithTheSameObject() throws Exception {
        try (OpenCancel oc = new OpenCancel()) {
            ParkedOpen a = park(listenerOpen("Receiver.open"), oc);
            ParkedOpen b = park(listenerOpen("ManagedDemuxReceiver.fromUrl"), oc);
            try {
                oc.cancel();
                a.opening().join("Receiver.open");
                b.opening().join("ManagedDemuxReceiver.fromUrl");
                assertSrtClosed(a.opening().error());
                assertSrtClosed(b.opening().error());
            } finally {
                rescue(a.port(), a.opening());
                rescue(b.port(), b.opening());
            }
        }
    }

    @Test
    void isCancelledFollowsCancelAndCancelIsIdempotent() {
        try (OpenCancel oc = new OpenCancel()) {
            assertFalse(oc.isCancelled());
            oc.cancel();
            oc.cancel();
            assertTrue(oc.isCancelled());
        }
    }

    // ----------------------------------------------------------------------------------------
    // An object cancelled before the open: refused, nothing opened
    // ----------------------------------------------------------------------------------------

    @ParameterizedTest
    @ValueSource(strings = {"Receiver.open", "DemuxReceiver.open", "ManagedReceiver.fromUrl",
                            "ManagedDemuxReceiver.fromUrl"})
    void aCancelledObjectRefusesAListenerOpenAndBindsNothing(String name) throws Exception {
        try (OpenCancel oc = new OpenCancel()) {
            oc.cancel();
            int port = freeUdpPort();
            Exception e = assertThrows(Exception.class,
                () -> listenerOpen(name).open("srt://127.0.0.1:" + port + "?mode=listener", oc));
            assertSrtClosed(e);
            assertFalse(udpPortBound(port), "the refused open left its port bound");
        }
    }

    /**
     * Nothing reaches the dial target: once the refused open has returned, the test sends a
     * marker datagram to the same port, and the marker is the first datagram there. A dial
     * sends its handshake before the open returns, so it would be queued ahead of the marker.
     */
    @ParameterizedTest
    @ValueSource(strings = {"Sender.open", "MuxSender.open", "ManagedSender.fromUrl",
                            "ManagedMuxSender.fromUrl", "ManagedDemuxReceiver.fromUrl"})
    void aCancelledObjectRefusesACallerOpenWithoutDialling(String name) throws Exception {
        try (OpenCancel oc = new OpenCancel();
             DatagramSocket target = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0))) {
            oc.cancel();
            target.setSoTimeout((int) DEADLINE_MS);
            int port = target.getLocalPort();
            Exception e = assertThrows(Exception.class, () -> callerOpen(name).open(
                "srt://127.0.0.1:" + port + "?mode=caller&conntimeo=1000", oc));
            assertSrtClosed(e);
            byte[] marker = "marker".getBytes(java.nio.charset.StandardCharsets.US_ASCII);
            try (DatagramSocket s = new DatagramSocket()) {
                s.send(new DatagramPacket(marker, marker.length, target.getLocalSocketAddress()));
            }
            DatagramPacket first = new DatagramPacket(new byte[2048], 2048);
            target.receive(first);
            assertArrayEquals(marker, Arrays.copyOf(first.getData(), first.getLength()),
                "the refused open dialled: its handshake arrived first");
        }
    }

    /** A URL of each scheme with a cancelled object: every shell open throws that scheme's CLOSED. */
    @ParameterizedTest
    @ValueSource(strings = {"srt", "rtp", "udp", "tcp", "tcps", "rist"})
    void aCancelledObjectThrowsTheSchemesClosed(String scheme) throws Exception {
        try (OpenCancel oc = new OpenCancel()) {
            oc.cancel();
            int port = freeUdpPort();
            String url = scheme.equals("srt") ? "srt://127.0.0.1:" + port + "?mode=listener"
                : scheme.equals("rist") ? "rist://@127.0.0.1:" + port
                : scheme + "://127.0.0.1:" + port;
            Class<? extends BindingException> cls =
                ShellPeers.errorClass(scheme.equals("tcps") ? "tcp" : scheme);
            List<Open> opens = List.of(
                Receiver::open,
                (u, c) -> DemuxReceiver.open(u, DemuxerConfig.builder().build(), c),
                Sender::open,
                (u, c) -> MuxSender.open(u, roundtripConfig(), c));
            for (Open open : opens) {
                Exception e = assertThrows(Exception.class, () -> open.open(url, oc));
                assertClosed(e, cls);
            }
        }
    }

    /** The object is checked before the URL is parsed. */
    @Test
    void aBadUrlWithACancelledObjectIsClosed() {
        try (OpenCancel oc = new OpenCancel()) {
            oc.cancel();
            String bad = "srt://127.0.0.1:notaport?mode=listener&no_such_key=1";
            List<Callable<?>> calls = new ArrayList<>(List.of(
                () -> Receiver.open(bad, oc),
                () -> Sender.open(bad, oc)));
            // The managed shells are SRT only, so any other URL is SRT's CLOSED too.
            for (String url : List.of(bad, "udp://127.0.0.1:1", "rtp://127.0.0.1:1",
                                      "http://127.0.0.1:1")) {
                calls.add(() -> ManagedReceiver.fromUrl(url, null, oc));
                calls.add(() -> ManagedSender.fromUrl(url, null, oc));
                calls.add(() -> ManagedDemuxReceiver.fromUrl(
                    url, DemuxerConfig.builder().build(), null, oc));
                calls.add(() -> ManagedMuxSender.fromUrl(url, roundtripConfig(), null, oc));
            }
            for (Callable<?> call : calls) {
                assertSrtClosed(assertThrows(Exception.class, call::call));
            }
        }
    }

    /** No transport means no CLOSED to throw: the one exception to the object being checked first. */
    @Test
    void anUnsupportedSchemeIsIllegalArgumentEvenWithACancelledObject() {
        try (OpenCancel oc = new OpenCancel()) {
            oc.cancel();
            IllegalArgumentException s = assertThrows(IllegalArgumentException.class,
                () -> Sender.open("http://127.0.0.1:1", oc));
            assertTrue(s.getMessage().contains("unsupported URL scheme"), s.getMessage());
            IllegalArgumentException r = assertThrows(IllegalArgumentException.class,
                () -> Receiver.open("http://127.0.0.1:1", oc));
            assertTrue(r.getMessage().contains("unsupported URL scheme"), r.getMessage());
        }
    }

    // ----------------------------------------------------------------------------------------
    // The outcome rule: an open that finishes after the cancel throws CLOSED
    // ----------------------------------------------------------------------------------------

    /**
     * A cancel during a caller-mode dial does not interrupt it; the dial to a port nothing
     * answers then times out, and the open reports the cancel instead. The dial is proven in
     * progress by its first handshake datagram arriving at that port, past the open's check
     * of the object.
     */
    @ParameterizedTest
    @ValueSource(strings = {"Sender.open", "ManagedSender.fromUrl", "ManagedDemuxReceiver.fromUrl"})
    @Timeout(60)
    void aDialThatFailsAfterTheCancelReportsClosed(String name) throws Exception {
        try (OpenCancel oc = new OpenCancel();
             DatagramSocket dead = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0))) {
            dead.setSoTimeout((int) DEADLINE_MS);
            int port = dead.getLocalPort();
            Opening o = new Opening(() -> callerOpen(name).open(
                "srt://127.0.0.1:" + port + "?mode=caller&conntimeo=3000", oc));
            try {
                dead.receive(new DatagramPacket(new byte[2048], 2048));
                assertFalse(o.result.isDone());
                oc.cancel();
                o.thread.join(DEADLINE_MS + 3_000);
                assertFalse(o.thread.isAlive(), "the dial did not end");
                assertSrtClosed(o.error());
            } finally {
                o.thread.join(DEADLINE_MS);
                o.closeResult();
            }
        }
    }

    /** An SRT control packet (top bit set) of type 5, SHUTDOWN. */
    private static final byte[] SRT_SHUTDOWN = {(byte) 0x80, 0x05};

    /**
     * A UDP relay between a caller and an SRT listener that holds the caller's first datagram
     * until {@link #release()}: the caller's dial is then provably in progress, and it
     * completes only once released.
     */
    private static final class HeldRelay implements AutoCloseable {
        final DatagramSocket sock;
        final SocketAddress target;
        final CountDownLatch dialSeen = new CountDownLatch(1);
        final CountDownLatch shutdownSeen = new CountDownLatch(1);
        private volatile boolean go;
        private volatile boolean stop;
        private final Thread thread;

        HeldRelay(int targetPort) throws java.io.IOException {
            sock = new DatagramSocket(new InetSocketAddress("127.0.0.1", 0));
            sock.setSoTimeout(50);
            target = new InetSocketAddress("127.0.0.1", targetPort);
            thread = new Thread(this::run, "open-cancel-relay");
            thread.setDaemon(true);
            thread.start();
        }

        int port() { return sock.getLocalPort(); }

        void release() { go = true; }

        @Override public void close() throws InterruptedException {
            stop = true;
            go = true;
            thread.join(DEADLINE_MS);
            sock.close();
        }

        private void send(byte[] data, int len, SocketAddress to) throws java.io.IOException {
            sock.send(new DatagramPacket(data, len, to));
        }

        private void run() {
            SocketAddress caller = null;
            byte[] held = null;
            byte[] buf = new byte[65536];
            try {
                while (!stop) {
                    DatagramPacket pk = new DatagramPacket(buf, buf.length);
                    try {
                        sock.receive(pk);
                    } catch (SocketTimeoutException e) {
                        if (held != null && go) {
                            send(held, held.length, target);
                            held = null;
                        }
                        continue;
                    }
                    byte[] data = Arrays.copyOf(pk.getData(), pk.getLength());
                    if (pk.getSocketAddress().equals(target)) {
                        if (caller != null) send(data, data.length, caller);
                        continue;
                    }
                    caller = pk.getSocketAddress();
                    if (data.length >= 2 && data[0] == SRT_SHUTDOWN[0] && data[1] == SRT_SHUTDOWN[1]) {
                        shutdownSeen.countDown();
                    }
                    if (!go) {
                        held = data;
                        dialSeen.countDown();
                        continue;
                    }
                    if (held != null) {
                        send(held, held.length, target);
                        held = null;
                    }
                    send(data, data.length, target);
                }
            } catch (java.io.IOException e) {
                // socket closed
            }
        }
    }

    /**
     * The cancel lands while the dial is held in progress; the dial then connects, but the
     * open throws CLOSED, closes the connection and hands nothing out.
     */
    @Test
    @Timeout(60)
    void aDialThatSucceedsAfterTheCancelIsClosedAgain() throws Exception {
        try (Listener lst = new Builder("srt://127.0.0.1:0").listener().listen();
             HeldRelay relay = new HeldRelay(lst.localAddr().getPort());
             OpenCancel oc = new OpenCancel()) {
            Opening o = new Opening(() -> Sender.open(
                "srt://127.0.0.1:" + relay.port() + "?conntimeo=5000", oc));
            try {
                assertTrue(relay.dialSeen.await(DEADLINE_MS, TimeUnit.MILLISECONDS), "the caller never dialled");
                assertFalse(o.result.isDone());
                oc.cancel();
                relay.release();
                o.join("Sender.open");
                assertSrtClosed(o.error());
                assertNull(o.value());
                // The dial did connect, and the open closed the connection again:
                // libsrt sends a SHUTDOWN only on a connected socket.
                assertTrue(relay.shutdownSeen.await(DEADLINE_MS, TimeUnit.MILLISECONDS),
                    "no SHUTDOWN: the dial never connected");
            } finally {
                o.thread.join(DEADLINE_MS);
                o.closeResult();
            }
        }
    }

    /**
     * The object's cancel() sets the latch, then blocks in an earlier open's wake before it
     * reaches the managed receiver's accept. A peer connects meanwhile and the accept
     * succeeds, but the open throws CLOSED and closes the connection again: the peer sees
     * it end.
     */
    @ParameterizedTest
    @ValueSource(strings = {"ManagedReceiver.fromUrl", "ManagedDemuxReceiver.fromUrl"})
    @EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
    @Timeout(60)
    void anAcceptThatSucceedsAfterTheCancelIsClosedAgain(String name) throws Exception {
        long probe = CancelProbe.nOpen();
        OpenCancel oc = new OpenCancel();
        try {
            // Registered first, so cancel() wakes it first.
            CancelProbe.nHoldOpen(probe, oc);
            ParkedOpen p = park(listenerOpen(name), oc);
            Thread canceller = new Thread(oc::cancel, "open-cancel-held-cancel");
            canceller.setDaemon(true);
            Opening peer = null;
            try {
                canceller.start();
                assertTrue(CancelProbe.nAwaitEntered(probe, DEADLINE_MS), "cancel() never reached the held wake");
                assertTrue(oc.isCancelled());
                assertFalse(p.opening().result.isDone(), "the accept ended before a peer connected");
                peer = new Opening(() -> {
                    Builder dial = new Builder("srt://127.0.0.1:" + p.port()).caller().connectTimeoutMs(3000);
                    try (Receiver rx = dial.connect().intoReceiver()) {
                        return rx.recvBytes();
                    }
                });
                // Joined while cancel() is still held: only the peer can have ended the accept.
                p.opening().join(name);
                assertTrue(canceller.isAlive(), "cancel() was released before the open returned");
                assertSrtClosed(p.opening().error());
                assertNull(p.opening().value());
                peer.join("the peer");
                assertInstanceOf(SrtException.class, peer.error(),
                    "the peer's connection did not end: " + peer.error());
            } finally {
                CancelProbe.nRelease(probe);
                canceller.join(DEADLINE_MS);
                rescue(p.port(), p.opening());
                if (peer != null) {
                    peer.thread.join(DEADLINE_MS);
                    peer.closeResult();
                }
            }
            assertFalse(canceller.isAlive(), "cancel() did not return after the wake was released");
        } finally {
            oc.close();
            CancelProbe.nClose(probe);
        }
    }

    // ----------------------------------------------------------------------------------------
    // A cancel after the open returned: the shell is untouched
    // ----------------------------------------------------------------------------------------

    @Test
    @Timeout(60)
    void aCancelAfterTheOpenReturnedLeavesTheShellsWorking() throws Exception {
        try (OpenCancel oc = new OpenCancel(); ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            ParkedOpen p = park(listenerOpen("Receiver.open"), oc);
            try {
                Sender tx = c.add(Sender.open("srt://127.0.0.1:" + p.port(), oc));
                p.opening().join("Receiver.open");
                assertNull(p.opening().error());
                Receiver rx = c.add((Receiver) p.opening().value());
                oc.cancel();
                try (CancelHandle rh = rx.cancelHandle(); CancelHandle th = tx.cancelHandle()) {
                    assertFalse(rh.isCancelled());
                    assertFalse(th.isCancelled());
                }
                for (int i = 0; i < 7; i++) tx.sendBytes(ShellPeers.TS_PACKET);
                tx.flush();
                byte[] got = rx.recvBytes();
                assertEquals(188, got.length);
                assertEquals(0x47, got[0]);
            } finally {
                rescue(p.port(), p.opening());
            }
        }
    }

    /** Connect a caller, retrying while the managed listener re-binds. */
    private static ManagedSender connectWithRetry(String url) throws Exception {
        long end = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(DEADLINE_MS);
        while (true) {
            try {
                return ManagedSender.fromUrl(url);
            } catch (SrtException e) {
                if (System.nanoTime() > end) throw e;
                Thread.sleep(50);
            }
        }
    }

    @Test
    @Timeout(90)
    void aCancelAfterAManagedReceiverOpenedLeavesItsReconnectsGoing() throws Exception {
        try (OpenCancel oc = new OpenCancel()) {
            ParkedOpen p = park(listenerOpen("ManagedReceiver.fromUrl"), oc);
            String caller = "srt://127.0.0.1:" + p.port();
            List<byte[]> got = java.util.Collections.synchronizedList(new ArrayList<>());
            CompletableFuture<Throwable> pumpEnd = new CompletableFuture<>();
            ManagedSender tx = null;
            ManagedReceiver rx = null;
            Thread pump = null;
            try {
                tx = ManagedSender.fromUrl(caller);
                p.opening().join("ManagedReceiver.fromUrl");
                assertNull(p.opening().error());
                rx = (ManagedReceiver) p.opening().value();
                oc.cancel();
                try (CancelHandle h = rx.cancelHandle()) {
                    assertFalse(h.isCancelled());
                }
                ManagedReceiver r = rx;
                pump = new Thread(() -> {
                    try {
                        while (true) got.add(r.recvBytes());
                    } catch (Throwable e) {
                        pumpEnd.complete(e);
                    }
                }, "open-cancel-pump");
                pump.setDaemon(true);
                pump.start();
                for (int i = 0; i < 7; i++) tx.sendBytes(ShellPeers.TS_PACKET);
                tx.flush();
                waitFor("data before the reconnect", () -> !got.isEmpty() || pumpEnd.isDone());
                assertFalse(pumpEnd.isDone(), () -> "the receive ended: " + pumpEnd.getNow(null));

                // The peer leaves; the receiver re-accepts on the slot the object no longer
                // reaches, and a new peer's data arrives.
                tx.close();
                tx = null;
                ManagedReceiver watched = rx;
                waitFor("a reconnect attempt", () -> watched.reconnectAttempts() >= 1 || pumpEnd.isDone());
                int before = got.size();
                tx = connectWithRetry(caller);
                ManagedSender t = tx;
                waitFor("data after the reconnect", () -> {
                    if (got.size() > before || pumpEnd.isDone()) return true;
                    try {
                        for (int i = 0; i < 7; i++) t.sendBytes(ShellPeers.TS_PACKET);
                        t.flush();
                    } catch (SrtException e) {
                        // the link is still coming up; try again on the next poll
                    }
                    return false;
                });
                assertFalse(pumpEnd.isDone(), () -> "the receive ended: " + pumpEnd.getNow(null));
                rx.close();
                pump.join(DEADLINE_MS);
                assertFalse(pump.isAlive());
            } finally {
                rescue(p.port(), p.opening());
                if (tx != null) tx.close();
                if (rx != null) rx.close();
                if (pump != null) pump.join(DEADLINE_MS);
            }
        }
    }

    // ----------------------------------------------------------------------------------------
    // close() frees the Java object only
    // ----------------------------------------------------------------------------------------

    /**
     * {@code close()} while an open is parked with the object does not end the open: the open
     * keeps its own reference. The test then ends the open the ordinary way, by connecting a
     * peer, and the open returns a working receiver.
     */
    @Test
    @Timeout(60)
    void closeWhileAnOpenIsParkedDoesNotEndIt() throws Exception {
        OpenCancel oc = new OpenCancel();
        ParkedOpen p = park(listenerOpen("Receiver.open"), oc);
        try (ShellPeers.Cleanup c = new ShellPeers.Cleanup()) {
            oc.close();
            oc.close(); // idempotent
            assertEquals("OpenCancel is closed",
                assertThrows(IllegalStateException.class, oc::cancel).getMessage());
            assertThrows(IllegalStateException.class, oc::isCancelled);
            assertThrows(IllegalStateException.class,
                () -> Receiver.open("udp://127.0.0.1:" + freeUdpPort(), oc));
            assertTrue(udpPortBound(p.port()));
            assertFalse(p.opening().result.isDone(), "close() ended the parked open");

            Sender tx = c.add(Sender.open("srt://127.0.0.1:" + p.port()));
            p.opening().join("Receiver.open");
            assertNull(p.opening().error(), () -> "the open failed: " + p.opening().error());
            Receiver rx = c.add((Receiver) p.opening().value());
            for (int i = 0; i < 7; i++) tx.sendBytes(ShellPeers.TS_PACKET);
            tx.flush();
            assertEquals(188, rx.recvBytes().length);
        } finally {
            rescue(p.port(), p.opening());
        }
    }

    // ----------------------------------------------------------------------------------------
    // Without an object nothing changes
    // ----------------------------------------------------------------------------------------

    @Test
    void aNullObjectIsTheSameAsNoArgument() throws Exception {
        int port = freeUdpPort();
        try (Receiver rx = Receiver.open("udp://127.0.0.1:" + port, null);
             Sender tx = Sender.open("udp://127.0.0.1:" + port, null)) {
            for (int i = 0; i < 7; i++) tx.sendBytes(ShellPeers.TS_PACKET);
            tx.flush();
            assertEquals(0x47, rx.recvBytes()[0]);
        }
    }

    @Test
    void openCancelIsAFinalPublicClass() throws Exception {
        assertTrue(Modifier.isFinal(OpenCancel.class.getModifiers()));
        assertTrue(Modifier.isPublic(OpenCancel.class.getConstructor().getModifiers()));
    }

    // ----------------------------------------------------------------------------------------
    // cancel() holds no lock across a slot cancel that blocks
    // ----------------------------------------------------------------------------------------

    /**
     * An object with one open whose wake blocks until the test releases it, as an SRT
     * listener close can: {@code isCancelled()} and {@code close()} on the same object answer
     * while a thread is inside {@code cancel()}.
     */
    @Test
    @EnabledIfSystemProperty(named = "tst.jniTestHooks", matches = "true")
    @Timeout(60)
    void cancelHoldsNoLockAcrossABlockingWake() throws Exception {
        long probe = CancelProbe.nOpen();
        OpenCancel oc = new OpenCancel();
        try {
            CancelProbe.nHoldOpen(probe, oc);
            Thread canceller = new Thread(oc::cancel, "open-cancel-parked-cancel");
            canceller.setDaemon(true);
            canceller.start();
            try {
                assertTrue(CancelProbe.nAwaitEntered(probe, DEADLINE_MS), "cancel() never reached the open's wake");
                assertTrue(ShellPeers.within(DEADLINE_MS, "isCancelled()", oc::isCancelled),
                    "the latch is set before the wake blocks");
                ShellPeers.within(DEADLINE_MS, "close()", () -> {
                    oc.close();
                    return null;
                });
            } finally {
                CancelProbe.nRelease(probe);
                canceller.join(DEADLINE_MS);
            }
            assertFalse(canceller.isAlive(), "cancel() did not return after the wake was released");
        } finally {
            oc.close();
            CancelProbe.nClose(probe);
        }
    }
}
