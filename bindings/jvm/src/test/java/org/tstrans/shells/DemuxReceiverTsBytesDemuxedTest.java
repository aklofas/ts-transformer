package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.reflect.Modifier;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.srt.ManagedDemuxReceiver;

/** {@link DemuxReceiver#tsBytesDemuxed()} on a live TCP receiver. */
@Timeout(value = 60, threadMode = Timeout.ThreadMode.SEPARATE_THREAD)
class DemuxReceiverTsBytesDemuxedTest {

    /** Writes a clean stream, ends it, and drains the receiver to the end. */
    private static void feedAndDrain(DemuxReceiverTestSupport.Pair p) throws Exception {
        p.write(DemuxReceiverTestSupport.eventPackets());
        p.peer.shutdownOutput();
        List<DemuxEvent> events = new ArrayList<>();
        Object r = DemuxReceiverTestSupport.await(DemuxReceiverTestSupport.drainOnDaemon(p.rx, events));
        assertFalse(r instanceof Throwable, String.valueOf(r));
    }

    @Test
    void tsBytesDemuxedCountsWhatTheDemuxerWasFed() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            feedAndDrain(p);
            long n = p.rx.tsBytesDemuxed();
            assertEquals(DemuxReceiverTestSupport.eventPackets().length, n);
            assertEquals(0, n % 188);
        }
    }

    @Test
    void tsBytesDemuxedAnswersWhileNextIsParked() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            CompletableFuture<Object> parked = DemuxReceiverTestSupport.recvOnDaemon(p.rx);
            Thread.sleep(TestSupport.scaledMs(200));
            CompletableFuture<Long> answer = CompletableFuture.supplyAsync(p.rx::tsBytesDemuxed);
            assertEquals(0L, answer.get(TestSupport.scaledMs(2_000), TimeUnit.MILLISECONDS));
            assertFalse(parked.isDone(), "the receive stays parked");
            p.rescue.cancel();
            DemuxReceiverTestSupport.await(parked);
        }
    }

    @Test
    void tsBytesDemuxedReturnsTheLastValueAfterClose() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            feedAndDrain(p);
            long n = p.rx.tsBytesDemuxed();
            assertTrue(n > 0);
            p.rx.close();
            assertEquals(n, p.rx.tsBytesDemuxed());
            p.rx.close();
            assertEquals(n, p.rx.tsBytesDemuxed());
        }
    }

    @Test
    void tsBytesDemuxedNeverDecreasesWhileAnotherThreadCloses() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            feedAndDrain(p);
            long n = p.rx.tsBytesDemuxed();
            AtomicBoolean stop = new AtomicBoolean();
            CountDownLatch firstRead = new CountDownLatch(1);
            CompletableFuture<Long> reader = new CompletableFuture<>();
            Thread t = new Thread(() -> {
                try {
                    long prev = 0;
                    do {
                        long v = p.rx.tsBytesDemuxed();
                        if (v < prev) throw new AssertionError("went from " + prev + " to " + v);
                        prev = v;
                        firstRead.countDown();
                    } while (!stop.get());
                    reader.complete(prev);
                } catch (Throwable e) {
                    reader.completeExceptionally(e);
                }
            }, "ts-bytes-reader");
            t.setDaemon(true);
            t.start();
            assertTrue(firstRead.await(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS));
            p.rx.close();
            // Keep reading after the close so the closed path is exercised too.
            Thread.sleep(TestSupport.scaledMs(50));
            stop.set(true);
            assertEquals(n, reader.get(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS));
            assertEquals(n, p.rx.tsBytesDemuxed());
        }
    }

    /** A receiver whose native teardown waits on a latch, so a second closer arrives mid-close. */
    private static final class GatedCloseReceiver extends DemuxReceiver {
        final CountDownLatch inNativeClose = new CountDownLatch(1);
        final CountDownLatch releaseNativeClose = new CountDownLatch(1);

        GatedCloseReceiver(long handle) { super(handle); }

        @Override protected void nativeClose(long h) {
            inNativeClose.countDown();
            try {
                releaseNativeClose.await(TestSupport.scaledMs(10_000), TimeUnit.MILLISECONDS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            super.nativeClose(h);
        }
    }

    @Test
    void aLosingCloseReturnsOnlyAfterTheFinalCountIsPublished() throws Exception {
        try (ServerSocket server = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            GatedCloseReceiver rx = new GatedCloseReceiver(
                DemuxReceiver.openHandle("tcp://127.0.0.1:" + server.getLocalPort()));
            try (Socket peer = server.accept()) {
                byte[] packets = DemuxReceiverTestSupport.eventPackets();
                peer.getOutputStream().write(packets);
                peer.getOutputStream().flush();
                peer.shutdownOutput();
                List<DemuxEvent> events = new ArrayList<>();
                Object r = DemuxReceiverTestSupport.await(DemuxReceiverTestSupport.drainOnDaemon(rx, events));
                assertFalse(r instanceof Throwable, String.valueOf(r));
                // No tsBytesDemuxed() read yet: the Java-side cache is still 0.

                Thread winner = new Thread(rx::close, "winning-closer");
                winner.setDaemon(true);
                winner.start();
                assertTrue(rx.inNativeClose.await(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS));

                CompletableFuture<Long> loserSaw = new CompletableFuture<>();
                Thread loser = new Thread(() -> {
                    try {
                        rx.close();
                        loserSaw.complete(rx.tsBytesDemuxed());
                    } catch (Throwable e) {
                        loserSaw.completeExceptionally(e);
                    }
                }, "losing-closer");
                loser.setDaemon(true);
                loser.start();
                // Either the loser already returned (the defect) or it waits on the monitor.
                long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(TestSupport.scaledMs(5_000));
                while (!loserSaw.isDone() && loser.getState() != Thread.State.BLOCKED
                        && System.nanoTime() < deadline) {
                    Thread.sleep(5);
                }
                rx.releaseNativeClose.countDown();

                assertEquals((long) packets.length,
                    loserSaw.get(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS),
                    "a close() that returns must see the count the winning close published");
                winner.join(TestSupport.scaledMs(5_000));
            } finally {
                rx.releaseNativeClose.countDown();
                rx.close();
            }
        }
    }

    @Test
    void bothReceiverCloseOverridesAreSynchronized() throws Exception {
        for (Class<?> c : List.of(DemuxReceiver.class, ManagedDemuxReceiver.class)) {
            int mods = c.getDeclaredMethod("close").getModifiers();
            assertTrue(Modifier.isSynchronized(mods), c.getSimpleName() + ".close() must be synchronized");
        }
    }
}
