package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Muxer;

/**
 * Byte-sink callbacks on a live {@link DemuxReceiver}: a callback that throws ends the
 * receive, and a callback that calls back into its receiver is refused or, for
 * {@code close()}, deferred. TCP so the listener binds port 0 and the port is read back.
 * A pull that could deadlock runs on a daemon thread, so a regression fails the test
 * instead of wedging the suite.
 */
@Timeout(value = 60, threadMode = Timeout.ThreadMode.SEPARATE_THREAD)
class DemuxReceiverByteSinkFailureTest {

    /** {@code n} null packets (PID 0x1FFF): the demuxer never produces an event from them. */
    static byte[] nullPackets(int n) {
        byte[] out = new byte[n * 188];
        Arrays.fill(out, (byte) 0xFF);
        for (int i = 0; i < n; i++) {
            out[i * 188] = 0x47;
            out[i * 188 + 1] = 0x1F;
            out[i * 188 + 2] = (byte) 0xFF;
            out[i * 188 + 3] = 0x10;
        }
        return out;
    }

    /** PMT PID of {@link TestSupport#roundtripConfig()}. */
    static final int PMT_PID = 0x1000;

    /** PAT, PMT and three video PES of {@link TestSupport#roundtripConfig()}: events follow. */
    static byte[] eventPackets() throws Exception {
        try (Muxer m = new Muxer(TestSupport.roundtripConfig())) {
            for (int i = 0; i < 3; i++) {
                m.pushVideo(TestSupport.syntheticH264Idr(), i * 3000L, true);
            }
            return TestSupport.drain(m);
        }
    }

    static int pid(byte[] pkt) {
        return ((pkt[1] & 0x1F) << 8) | (pkt[2] & 0xFF);
    }

    /** A TCP receiver, its connected peer, and a cancel handle taken before any sink. */
    static final class Pair implements AutoCloseable {
        final ServerSocket server;
        final DemuxReceiver rx;
        final java.net.Socket peer;
        final CancelHandle rescue;

        Pair() throws Exception {
            server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
            rx = DemuxReceiver.open("tcp://127.0.0.1:" + server.getLocalPort());
            peer = server.accept();
            rescue = rx.cancelHandle();
        }

        void write(byte[] bytes) throws IOException {
            peer.getOutputStream().write(bytes);
            peer.getOutputStream().flush();
        }

        @Override public void close() throws IOException {
            rescue.cancel();
            rx.close();
            peer.close();
            server.close();
        }
    }

    /** One {@code recvEvent()} on a daemon thread: the event (as an Optional) or the throwable. */
    static CompletableFuture<Object> recvOnDaemon(DemuxReceiver rx) {
        CompletableFuture<Object> f = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try {
                f.complete(Optional.ofNullable(rx.recvEvent()));
            } catch (Throwable e) {
                f.complete(e);
            }
        }, "sink-recv");
        t.setDaemon(true);
        t.start();
        return f;
    }

    /** Iterate {@code rx} to its end on a daemon thread: the events, or the throwable. */
    static CompletableFuture<Object> drainOnDaemon(DemuxReceiver rx, List<DemuxEvent> events) {
        CompletableFuture<Object> f = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try {
                for (DemuxEvent e : rx) events.add(e);
                f.complete(events);
            } catch (Throwable e) {
                f.complete(e);
            }
        }, "sink-drain");
        t.setDaemon(true);
        t.start();
        return f;
    }

    static Object await(CompletableFuture<Object> f) throws Exception {
        return f.get(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS);
    }

    /** A sink failure on a quiet stream must surface without waiting for another event. */
    @Test
    void throwingSinkEndsTheReceiveAndIsRethrown() throws Exception {
        try (ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress());
             DemuxReceiver rx = DemuxReceiver.open("tcp://127.0.0.1:" + server.getLocalPort());
             java.net.Socket peer = server.accept()) {
            CancelHandle rescue = rx.cancelHandle();
            RuntimeException boom = new IllegalStateException("sink failed");
            AtomicInteger calls = new AtomicInteger();
            rx.addByteSink(pkt -> { calls.incrementAndGet(); throw boom; });
            AtomicBoolean watchdogFired = new AtomicBoolean();
            Thread watchdog = new Thread(() -> {
                try {
                    Thread.sleep(TestSupport.scaledMs(5_000));
                } catch (InterruptedException e) {
                    return;
                }
                watchdogFired.set(true);
                rescue.cancel();
            }, "sink-watchdog");
            watchdog.setDaemon(true);
            watchdog.start();

            peer.getOutputStream().write(nullPackets(16));
            peer.getOutputStream().flush();

            Throwable thrown = assertThrows(Throwable.class, rx::recvEvent);
            watchdog.interrupt();
            assertSame(boom, thrown);
            assertFalse(watchdogFired.get(), "next() waited for an event instead of ending on the sink failure");
            assertTrue(rescue.isCancelled(), "the failed receive is ended, not resumable");
            assertEquals(1, calls.get(), "the sink is not called again after it threw");
        }
    }

    /** A slot-taking call from the receiver's own sink is refused instead of deadlocking. */
    @ParameterizedTest
    @ValueSource(strings = {"stats", "socketStats", "lastSeenMicros", "addByteSink", "recvEvent"})
    void sinkCallingASlotTakingMethodOnItsOwnReceiverIsRejected(String call) throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            AtomicReference<Throwable> seen = new AtomicReference<>();
            rx.addByteSink(pkt -> {
                if (seen.get() != null) return;
                try {
                    switch (call) {
                        case "stats" -> rx.stats();
                        case "socketStats" -> rx.socketStats();
                        case "lastSeenMicros" -> rx.lastSeenMicros(PMT_PID);
                        case "addByteSink" -> rx.addByteSink(q -> { });
                        default -> rx.recvEvent();
                    }
                    seen.set(new AssertionError(call + "() returned"));
                } catch (Throwable t) {
                    seen.set(t);
                }
            });
            CompletableFuture<Object> r = recvOnDaemon(rx);
            p.write(nullPackets(16));
            assertTrue(TestSupport.awaitNonNull(seen, TestSupport.scaledMs(5_000)),
                "the sink deadlocked on " + call + "()");
            assertInstanceOf(IllegalStateException.class, seen.get());
            assertTrue(seen.get().getMessage().contains("own byte sink"), seen.get().getMessage());
            p.rescue.cancel();
            await(r);
        }
    }

    /** {@code close()} from the sink, with no event in flight, ends that very pull. */
    @Test
    void sinkCallingCloseEndsTheInFlightNext() throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            AtomicBoolean closed = new AtomicBoolean();
            rx.addByteSink(pkt -> { if (closed.compareAndSet(false, true)) rx.close(); });
            CompletableFuture<Object> r = recvOnDaemon(rx);
            p.write(nullPackets(16));
            assertEquals(Optional.empty(), await(r),
                "next() must end (null) once the sink closed the receiver");
            assertFalse(rx.isAlive());
            assertThrows(IllegalStateException.class, rx::recvEvent);
        }
    }

    /** After its {@code close()} the sink is not called again, so a later throw cannot surface. */
    @Test
    void sinkThatClosesItsReceiverIsNotCalledAgain() throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            AtomicInteger calls = new AtomicInteger();
            rx.addByteSink(pkt -> {
                if (calls.incrementAndGet() > 1) throw new IllegalStateException("sink called after close");
                rx.close();
            });
            CompletableFuture<Object> r = recvOnDaemon(rx);
            p.write(nullPackets(16));
            assertEquals(Optional.empty(), await(r), "the deferred close must complete cleanly");
            assertEquals(1, calls.get());
            assertFalse(rx.isAlive());
        }
    }

    /** A sink registered after a sink failure would never run: the registration is refused. */
    @Test
    void addByteSinkAfterASinkFailureIsRefused() throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            RuntimeException boom = new IllegalStateException("sink failed");
            rx.addByteSink(pkt -> { throw boom; });
            CompletableFuture<Object> r = recvOnDaemon(rx);
            p.write(nullPackets(16));
            assertSame(boom, await(r));
            assertThrows(IllegalStateException.class, () -> rx.addByteSink(q -> { }));
        }
    }

    /** {@code close()} from the sink delivers the event that packet produced, then ends. */
    @Test
    void sinkCloseDeliversTheInFlightEventThenEndsTheIteration() throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            List<DemuxEvent> events = new ArrayList<>();
            AtomicInteger atClose = new AtomicInteger(-1);
            rx.addByteSink(pkt -> {
                if (pid(pkt) == PMT_PID && atClose.compareAndSet(-1, events.size())) rx.close();
            });
            CompletableFuture<Object> r = drainOnDaemon(rx, events);
            p.write(eventPackets());
            assertSame(events, await(r), "the iteration must end cleanly after the sink's close()");
            assertTrue(atClose.get() >= 0, "the sink never saw the PMT");
            assertEquals(atClose.get() + 1, events.size(), "exactly the in-flight event is delivered");
            assertInstanceOf(DemuxEvent.ProgramMap.class, events.get(events.size() - 1));
            assertFalse(rx.isAlive());
        }
    }

    /**
     * A's sink pulls B and B's sink calls back into A: A's slot is held further up the
     * thread, so A refuses {@code stats()} and defers {@code close()} as from its own sink.
     */
    @Test
    void callsThroughAnotherReceiversSinkAreRefusedOrDeferred() throws Exception {
        try (Pair a = new Pair(); Pair b = new Pair()) {
            AtomicReference<Throwable> aStats = new AtomicReference<>();
            AtomicReference<Object> pulled = new AtomicReference<>();
            b.rx.addByteSink(pkt -> {
                if (aStats.get() == null) {
                    try {
                        a.rx.stats();
                        aStats.set(new AssertionError("stats() returned"));
                    } catch (Throwable t) {
                        aStats.set(t);
                    }
                }
                a.rx.close();
            });
            a.rx.addByteSink(pkt -> {
                if (pulled.get() != null) return;
                try {
                    pulled.set(Optional.ofNullable(b.rx.recvEvent()));
                } catch (Throwable t) {
                    pulled.set(t);
                }
            });
            b.write(eventPackets());
            CompletableFuture<Object> r = recvOnDaemon(a.rx);
            a.write(nullPackets(16));
            assertEquals(Optional.empty(), await(r), "A's pull must end once B's sink closed A");
            assertInstanceOf(Optional.class, pulled.get(), String.valueOf(pulled.get()));
            assertTrue(((Optional<?>) pulled.get()).isPresent(), "B's pull delivers its event");
            assertInstanceOf(IllegalStateException.class, aStats.get());
            assertTrue(aStats.get().getMessage().contains("own byte sink"), aStats.get().getMessage());
            assertFalse(a.rx.isAlive());
            assertTrue(b.rx.isAlive());
        }
    }

    /**
     * A plain {@code close()} on another thread claims the handle (zeroes it) before it
     * cancels, so a sink that waits for that cancel and then calls {@code close()} finds
     * the handle at 0 and never reaches native code: its close is a no-op, the plain
     * close completes, and the next pull reports the closed receiver.
     */
    @Test
    void sinkCloseAfterAPlainCloseClaimedTheHandleIsANoOp() throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            CountDownLatch started = new CountDownLatch(1);
            Thread closer = new Thread(() -> {
                started.countDown();
                rx.close();
            }, "plain-closer");
            closer.setDaemon(true);
            AtomicBoolean fired = new AtomicBoolean();
            AtomicBoolean sawCancel = new AtomicBoolean();
            rx.addByteSink(pkt -> {
                if (!fired.compareAndSet(false, true)) return;
                closer.start();
                try {
                    started.await(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS);
                    long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(TestSupport.scaledMs(5_000));
                    while (!p.rescue.isCancelled() && System.nanoTime() < deadline) Thread.sleep(1);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                sawCancel.set(p.rescue.isCancelled());
                rx.close();
            });
            CompletableFuture<Object> r = recvOnDaemon(rx);
            p.write(nullPackets(16));
            await(r);
            closer.join(TestSupport.scaledMs(5_000));
            assertTrue(sawCancel.get(), "the plain close never cancelled");
            assertFalse(closer.isAlive(), "the plain close never finished");
            assertThrows(IllegalStateException.class, rx::recvEvent);
        }
    }

    /** The lock-free accessors answer from the sink while its pull holds the slot. */
    @Test
    void sinkMayUseTheLockFreeAccessors() throws Exception {
        try (Pair p = new Pair()) {
            DemuxReceiver rx = p.rx;
            AtomicReference<Object> seen = new AtomicReference<>();
            rx.addByteSink(pkt -> {
                if (seen.get() != null) return;
                try {
                    seen.set(List.of(rx.isAlive(), rx.cancelHandle().isCancelled(),
                        Optional.ofNullable(rx.endReason())));
                } catch (Throwable t) {
                    seen.set(t);
                }
            });
            CompletableFuture<Object> r = recvOnDaemon(rx);
            p.write(nullPackets(16));
            assertTrue(TestSupport.awaitNonNull(seen, TestSupport.scaledMs(5_000)), "the sink never ran");
            assertEquals(List.of(true, false, Optional.empty()), seen.get());
            p.rescue.cancel();
            await(r);
        }
    }
}
