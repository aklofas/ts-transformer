package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.srt.SrtSenderParkSupport.NON_READING_PEER_KNOBS;
import static org.tstrans.srt.SrtSenderParkSupport.awaitParked;
import static org.tstrans.srt.SrtSenderParkSupport.connectManagedMux;
import static org.tstrans.srt.SrtSenderParkSupport.connectPlainMux;
import static org.tstrans.srt.SrtSenderParkSupport.dataBlob;
import static org.tstrans.srt.SrtSenderParkSupport.managedParkPolicy;
import static org.tstrans.srt.SrtSenderParkSupport.nullTsBlock;
import static org.tstrans.srt.SrtSenderParkSupport.peerListener;
import static org.tstrans.srt.SrtSenderParkSupport.pump;

import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.Supplier;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.srt.SrtSenderParkSupport.Pump;

/**
 * Accessors that must answer while another thread is parked inside a native
 * call on the same object:
 *
 * <ul>
 *   <li>the stream-handle getters ({@code videoHandle()} … {@code dataHandle()})
 *       — the stream set is fixed when the sender is built, so they read a
 *       construction-time snapshot;</li>
 *   <li>{@code isAlive()} — a non-blocking probe; a parked call means the
 *       object is open, so it answers {@code true}.</li>
 * </ul>
 *
 * <p>Both used to take the resource lock the parked call holds, so a watchdog
 * thread asking "is it alive?" waited for as long as the call it was watching.
 *
 * <p>Every parked call runs on a daemon thread (a JUnit {@code @Timeout}
 * cannot interrupt a blocked native call) and every accessor is called
 * off-thread with a bounded wait; {@code close()} — cancel-first — is the
 * rescue on every path, so a regression fails instead of hanging.
 */
class AccessorsAnswerWhileParkedTest {

    private static final int LATENCY_MS = 120;
    /** Hang deadline for one accessor call; an answer takes microseconds. */
    private static final long ANSWER_DEADLINE_S = 5;

    /** Call {@code accessor} on a daemon thread; fail if it has not answered by the deadline. */
    private static <T> T answer(String what, Supplier<T> accessor) throws Exception {
        CompletableFuture<T> f = CompletableFuture.supplyAsync(accessor, r -> {
            Thread t = new Thread(r, "accessor");
            t.setDaemon(true);
            t.start();
        });
        try {
            return f.get(ANSWER_DEADLINE_S, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            fail(what + " waited behind the parked call instead of answering");
            throw te; // unreachable
        }
    }

    private static void assertStillParked(Pump pump, String what) {
        assertFalse(pump.end.isDone(), what + " ended before the accessors answered");
        assertNotEquals(0, pump.inFlightSince.get(), what + " returned before the accessors answered");
    }

    /** A reader parked in one native receive, with the entered / returned latches that prove it. */
    private static final class ParkedReader {
        final CountDownLatch entered = new CountDownLatch(1);
        final CompletableFuture<Throwable> returned = new CompletableFuture<>();

        ParkedReader(String name, SrtSenderParkSupport.Send receive) {
            Thread t = new Thread(() -> {
                entered.countDown();
                try {
                    receive.run();
                    returned.complete(null);
                } catch (Throwable e) {
                    returned.complete(e);
                }
            }, name);
            t.setDaemon(true);
            t.start();
        }

        void awaitParked(String what) throws Exception {
            assertTrue(entered.await(5, TimeUnit.SECONDS), what + " never started");
            Thread.sleep(300); // inside the native receive, holding the resource lock
            assertFalse(returned.isDone(), what + " returned on its own; the peer is silent");
        }

        void assertStillParked(String what) {
            assertFalse(returned.isDone(), what + " ended before the accessor answered");
        }
    }

    /** Connect a caller, retrying while the listener is between binds. */
    private static <T> T connect(String what, Connector<T> c) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return c.connect();
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError(what + " could not connect within 5 s", last);
    }

    @FunctionalInterface
    private interface Connector<T> {
        T connect() throws SrtException;
    }

    /** Open a listener-mode receiver on a daemon thread; the open blocks until a caller connects. */
    private static <T> CompletableFuture<T> listen(String name, Connector<T> open) {
        CompletableFuture<T> f = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try {
                f.complete(open.connect());
            } catch (Throwable e) {
                f.completeExceptionally(e);
            }
        }, name);
        t.setDaemon(true);
        t.start();
        return f;
    }

    // ── senders ────────────────────────────────────────────────────────────

    @Test
    @Timeout(90)
    void managedMuxSenderAnswersWhileSendIsParkedInReconnect() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300";

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        ManagedMuxSender sender = connectManagedMux(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
        try {
            List<Optional<?>> before = List.of(sender.videoHandle(), sender.klvHandle(),
                sender.audioHandle(), sender.subtitleHandle(), sender.dataHandle());
            assertTrue(before.get(0).isPresent(), "the fixture configures one video stream");

            byte[] idr = syntheticH264Idr();
            AtomicLong pts = new AtomicLong();
            Pump pump = pump("parked-managed-mux-sender",
                () -> sender.sendVideo(idr, pts.getAndAdd(3_000), true), 10);
            Thread.sleep(300);
            peer.close();                   // peer vanishes → Blocking reconnect
            awaitParked(pump, "sendVideo"); // parked in the backoff wait

            List<Optional<?>> during = answer("ManagedMuxSender.*Handle()", () -> List.of(
                sender.videoHandle(), sender.klvHandle(), sender.audioHandle(),
                sender.subtitleHandle(), sender.dataHandle()));
            boolean alive = answer("ManagedMuxSender.isAlive()", sender::isAlive);
            assertStillParked(pump, "sendVideo");
            assertEquals(before, during);
            assertTrue(alive, "a parked send means the sender is open");
        } finally {
            sender.close();
            peer.close();
        }
        assertFalse(sender.isAlive());
        assertThrows(IllegalStateException.class, sender::videoHandle);
    }

    @Test
    @Timeout(90)
    void managedSenderIsAliveAnswersWhileSendIsParkedInReconnect() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300";

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        ManagedSender sender = connect("ManagedSender",
            () -> ManagedSender.fromUrl(callerUrl, managedParkPolicy()));
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
        try {
            byte[] block = nullTsBlock();
            Pump pump = pump("parked-managed-sender", () -> sender.sendBytes(block), 10);
            Thread.sleep(300);
            peer.close();
            awaitParked(pump, "sendBytes");

            boolean alive = answer("ManagedSender.isAlive()", sender::isAlive);
            assertStillParked(pump, "sendBytes");
            assertTrue(alive, "a parked send means the sender is open");
        } finally {
            sender.close();
            peer.close();
        }
        assertFalse(sender.isAlive());
    }

    @Test
    @Timeout(90)
    void plainMuxSenderAnswersWhileSendIsParkedOnAFullSendBuffer() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS
            + NON_READING_PEER_KNOBS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        MuxSender sender = connectPlainMux(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS); // never reads
        try {
            List<Optional<?>> before = List.of(sender.videoHandle(), sender.klvHandle(),
                sender.audioHandle(), sender.subtitleHandle(), sender.dataHandle());
            assertTrue(before.get(0).isPresent() && before.get(4).isPresent(),
                "the fixture configures one video and one data stream");

            byte[] blob = dataBlob();
            AtomicLong pts = new AtomicLong();
            Pump pump = pump("parked-plain-mux-sender",
                () -> sender.sendData(blob, pts.getAndAdd(3_000)), 0);
            awaitParked(pump, "sendData");

            List<Optional<?>> during = answer("MuxSender.*Handle()", () -> List.of(
                sender.videoHandle(), sender.klvHandle(), sender.audioHandle(),
                sender.subtitleHandle(), sender.dataHandle()));
            boolean alive = answer("MuxSender.isAlive()", sender::isAlive);
            assertStillParked(pump, "sendData");
            assertEquals(before, during);
            assertTrue(alive, "a parked send means the sender is open");
        } finally {
            sender.close();
            peer.close();
        }
        assertFalse(sender.isAlive());
        assertThrows(IllegalStateException.class, sender::videoHandle);
    }

    @Test
    @Timeout(90)
    void plainSenderIsAliveAnswersWhileSendIsParkedOnAFullSendBuffer() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS
            + NON_READING_PEER_KNOBS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        Sender sender = connect("Sender", () -> Sender.fromUrl(callerUrl));
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
        try {
            byte[] block = nullTsBlock();
            Pump pump = pump("parked-plain-sender", () -> sender.sendBytes(block), 0);
            awaitParked(pump, "sendBytes");

            boolean alive = answer("Sender.isAlive()", sender::isAlive);
            assertStillParked(pump, "sendBytes");
            assertTrue(alive, "a parked send means the sender is open");
        } finally {
            sender.close();
            peer.close();
        }
        assertFalse(sender.isAlive());
    }

    // ── receivers: parked on a connected peer that sends nothing ───────────

    @Test
    @Timeout(60)
    void plainReceiverIsAliveAnswersWhileRecvIsParked() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> rxFuture = listen("receiver", () -> Receiver.fromUrl(listenUrl));
        Sender peer = connect("the silent peer", () -> Sender.fromUrl(callerUrl));
        Receiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        try {
            ParkedReader reader = new ParkedReader("parked-receiver", rx::recvBytes);
            reader.awaitParked("recvBytes()");
            boolean alive = answer("Receiver.isAlive()", rx::isAlive);
            reader.assertStillParked("recvBytes()");
            assertTrue(alive, "a parked receive means the receiver is open");
        } finally {
            rx.close();
            peer.close();
        }
        assertFalse(rx.isAlive());
    }

    @Test
    @Timeout(60)
    void managedReceiverIsAliveAnswersWhileRecvIsParked() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedReceiver> rxFuture = listen("managed-receiver",
            () -> ManagedReceiver.fromUrl(listenUrl, managedParkPolicy()));
        Sender peer = connect("the silent peer", () -> Sender.fromUrl(callerUrl));
        ManagedReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        try {
            ParkedReader reader = new ParkedReader("parked-managed-receiver", rx::recvBytes);
            reader.awaitParked("recvBytes()");
            boolean alive = answer("ManagedReceiver.isAlive()", rx::isAlive);
            reader.assertStillParked("recvBytes()");
            assertTrue(alive, "a parked receive means the receiver is open");
        } finally {
            rx.close();
            peer.close();
        }
        assertFalse(rx.isAlive());
    }

    @Test
    @Timeout(60)
    void managedDemuxReceiverIsAliveAnswersWhileNextIsParked() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedDemuxReceiver> rxFuture = listen("managed-demux-receiver",
            () -> ManagedDemuxReceiver.fromUrl(listenUrl, managedParkPolicy()));
        Sender peer = connect("the silent peer", () -> Sender.fromUrl(callerUrl));
        ManagedDemuxReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        try {
            ParkedReader reader = new ParkedReader("parked-managed-demux-receiver", () -> {
                for (DemuxEvent ignored : rx) {
                    // drain until the iteration ends
                }
            });
            reader.awaitParked("next()");
            boolean alive = answer("ManagedDemuxReceiver.isAlive()", rx::isAlive);
            reader.assertStillParked("next()");
            assertTrue(alive, "a parked receive means the receiver is open");
        } finally {
            rx.close();
            peer.close();
        }
        assertFalse(rx.isAlive());
    }
}
