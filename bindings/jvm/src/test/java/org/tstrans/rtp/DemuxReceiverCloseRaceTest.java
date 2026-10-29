package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.scaledMs;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;

/**
 * Memory-safety stress test for the rtp {@link DemuxReceiver#close()} racing a concurrent native
 * call. {@code shells.DemuxReceiver} has {@code cancelHandle()}; the test races {@code close()}
 * (cancel-first) against a parked {@code next()} and against fresh entries.
 *
 * <p>The receiver binds a UDP socket immediately (no peer required), so these are pure in-JVM
 * lifecycle tests with no live traffic. Two races are exercised:
 *
 * <ul>
 *   <li><b>close-vs-fresh-entry:</b> one thread hammers a cheap getter ({@code isAlive()} /
 *       {@code stats()}) while another calls {@code close()}; the leased {@code HandleRegistry}
 *       turns any race into either a clean run or a clean {@link IllegalStateException}.
 *   <li><b>close-vs-parked-next:</b> one thread parks in iteration (waiting for a datagram that
 *       never arrives); {@code close()} cancels the recv and frees only once it has unwound.
 * </ul>
 */
final class DemuxReceiverCloseRaceTest {

    /**
     * Bind an rtp receiver, retrying a few times if the discovered port's RTCP companion
     * ({@code port+1}) happens to be taken. Only an {@code IO} bind failure is retried; any other
     * failure, or exhausting the attempts, fails the test so it cannot pass without running.
     */
    private static DemuxReceiver tryReceiver() throws Exception {
        RtpException last = null;
        for (int attempt = 0; attempt < 8; attempt++) {
            try {
                return DemuxReceiver.open("rtp://127.0.0.1:" + freeUdpPort());
            } catch (RtpException e) {
                if (e.kind() != RtpException.Kind.IO) throw e;
                last = e; // port+1 was taken; try a different ephemeral port.
            }
        }
        throw new AssertionError("no rtp receiver could bind", last);
    }

    @Test
    @Timeout(30)
    void closeVsFreshEntryIsMemorySafe() throws Exception {
        int completed = 0;
        for (int i = 0; i < 100; i++) {
            DemuxReceiver rx = tryReceiver();

            CountDownLatch ready = new CountDownLatch(1);
            AtomicReference<Throwable> unexpected = new AtomicReference<>();

            Thread caller =
                new Thread(
                    () -> {
                        ready.countDown();
                        for (int k = 0; k < 1000; k++) {
                            try {
                                if (rx.isAlive()) {
                                    rx.stats();
                                }
                            } catch (IllegalStateException expected) {
                                // Handle claimed by close() — sanctioned.
                            } catch (Throwable t) {
                                unexpected.set(t);
                                return;
                            }
                        }
                    },
                    "rtp-rx-caller-" + i);
            caller.start();

            assertTrue(ready.await(2, TimeUnit.SECONDS), "caller thread never started");
            rx.close();

            caller.join(5000);
            assertFalse(caller.isAlive(), "caller thread did not finish (run " + i + ")");
            assertNull(unexpected.get(), "caller thread saw an unexpected failure (run " + i + ")");
            completed++;
        }
        assertEquals(100, completed);
    }

    @Test
    @Timeout(30)
    void closeWhileNextParkedIsMemorySafe() throws Exception {
        int completed = 0;
        for (int i = 0; i < 30; i++) {
            DemuxReceiver rx = tryReceiver();
            // Taken before the park so a regressed close() can still be unblocked.
            CancelHandle rescue = rx.cancelHandle();

            CountDownLatch aboutToIterate = new CountDownLatch(1);
            AtomicReference<Throwable> unexpected = new AtomicReference<>();

            Thread iter =
                new Thread(
                    () -> {
                        aboutToIterate.countDown();
                        try {
                            // Parks in next() waiting on a datagram that never arrives. close()
                            // from the main thread cancels the recv (waking it within ~100 ms).
                            for (var e : rx) {
                                assertNotNull(e);
                            }
                        } catch (RuntimeException wrapped) {
                            // A wrapped RtpException(CLOSED)/DemuxException is the sanctioned
                            // outcome of the cancelled recv.
                        } catch (Throwable t) {
                            unexpected.set(t);
                        }
                    },
                    "rtp-rx-iter-" + i);
            iter.start();

            assertTrue(aboutToIterate.await(2, TimeUnit.SECONDS), "iterator thread never started");
            Thread.sleep(20);
            try {
                CompletableFuture.runAsync(rx::close).get(scaledMs(5_000), TimeUnit.MILLISECONDS);
            } catch (TimeoutException hung) {
                rescue.cancel();
                fail("close() did not return (run " + i + ")");
            }

            iter.join(scaledMs(5_000));
            assertFalse(iter.isAlive(), "iterator did not unblock after close (run " + i + ")");
            assertNull(unexpected.get(), "iterator saw an unexpected failure (run " + i + ")");
            rescue.close();
            completed++;
        }
        assertEquals(30, completed);
    }
}
