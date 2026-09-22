package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.freeUdpPort;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;

/**
 * Memory-safety stress test for the rtp {@link DemuxReceiver#close()} racing a concurrent native
 * call. The RTP convenience wrapper has no {@code cancelHandle()}; {@code close()} is the only
 * sanctioned cross-thread stop and must safely interrupt a parked {@code next()}.
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
     * ({@code port+1}) happens to be taken. Returns null only if every attempt collided
     * (vanishingly unlikely) — callers skip that iteration.
     */
    private static DemuxReceiver tryReceiver() throws Exception {
        for (int attempt = 0; attempt < 8; attempt++) {
            try {
                return DemuxReceiver.fromUrl("rtp://127.0.0.1:" + freeUdpPort());
            } catch (Exception bindCollision) {
                // port+1 was taken; try a different ephemeral port.
            }
        }
        return null;
    }

    @Test
    @Timeout(30)
    void closeVsFreshEntryIsMemorySafe() throws Exception {
        for (int i = 0; i < 100; i++) {
            DemuxReceiver rx = tryReceiver();
            if (rx == null) continue;

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
        }
    }

    @Test
    @Timeout(30)
    void closeWhileNextParkedIsMemorySafe() throws Exception {
        for (int i = 0; i < 30; i++) {
            DemuxReceiver rx = tryReceiver();
            if (rx == null) continue;

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
            rx.close();

            iter.join(5000);
            assertFalse(iter.isAlive(), "iterator did not unblock after close (run " + i + ")");
            assertNull(unexpected.get(), "iterator saw an unexpected failure (run " + i + ")");
        }
    }
}
