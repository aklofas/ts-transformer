package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;

/**
 * The cancel-first {@code close()} contract of the managed srt receivers:
 * {@code close()} from another thread wakes a parked receive instead of waiting
 * behind it. Parity with tst-py's {@code close()} (see
 * {@code test_recv_end_reason_cancelled_after_close_while_parked}) and the C
 * ABI's {@code tst_managed_*_receiver_close}, which both cancel before tearing
 * down.
 *
 * <p>Both tests park a reader on a connected-but-silent peer, so nothing but the
 * cancel can end the native receive: under the previous contract {@code close()}
 * took the resource lock that receive held and never returned — the bounded
 * {@code get} on the close future is the red.
 *
 * <p>Readers are daemon threads on purpose: a JUnit {@code @Timeout} cannot
 * interrupt a blocked native read, so a reader that outlives a failed verdict
 * must not pin the JVM at exit. Every timeout path also sends a frame from the
 * still-connected peer to unpark the reader before failing.
 */
class SrtManagedCloseCancelsFirstTest {

    private static final int LATENCY_MS = 120;

    /** Connect a caller, retrying while the listener is between binds. */
    private static ManagedMuxSender connectSender(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return ManagedMuxSender.fromUrl(url, roundtripConfig());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /**
     * {@code close()} while {@code next()} is parked on another thread ends that
     * iteration with {@code SrtException(CLOSED)}, records
     * {@link RecvEndReason#CANCELLED}, and returns — the reason is then the
     * close-time snapshot {@link ManagedDemuxReceiver#endReason()} keeps serving.
     */
    @Test
    @Timeout(60)
    void closeWakesParkedIterationAndRecordsCancelled() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedDemuxReceiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        CountDownLatch startIterating = new CountDownLatch(1);

        Thread reader = new Thread(() -> {
            ManagedDemuxReceiver rx;
            try {
                rx = ManagedDemuxReceiver.fromUrl(listenUrl); // blocks until a peer connects
            } catch (Exception ex) {
                rxFuture.completeExceptionally(ex);
                endFuture.complete(ex);
                return;
            }
            rxFuture.complete(rx);
            try {
                startIterating.await();
                for (DemuxEvent ignored : rx) {
                    // drain until the iteration ends
                }
                endFuture.complete(null);
            } catch (RuntimeException re) {
                endFuture.complete(re.getCause() != null ? re.getCause() : re);
            } catch (InterruptedException ie) {
                Thread.currentThread().interrupt();
                endFuture.complete(ie);
            }
        });
        reader.setDaemon(true);
        reader.start();

        ManagedMuxSender sender = connectSender(callerUrl, 5_000);
        ManagedDemuxReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        startIterating.countDown();
        // Let the reader park inside the native receive: the peer is connected
        // and stays silent, so only a cancel can end that receive.
        Thread.sleep(500);
        assertNull(rx.endReason(), "recorded a reason before the stream ended");

        // The call under test, from a side thread so a regression to the old
        // wait-behind-the-parked-recv contract fails the bounded get below
        // instead of pinning this test thread until @Timeout.
        CompletableFuture<Void> closed = CompletableFuture.runAsync(rx::close);
        try {
            closed.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            rescue(sender, endFuture);
            fail("close() blocked for >5 s behind the parked iteration instead of cancelling it");
            return;
        }

        Throwable end;
        try {
            end = endFuture.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            rescue(sender, endFuture);
            fail("close() returned but the parked iteration did not end within 5 s");
            return;
        }
        reader.join(TimeUnit.SECONDS.toMillis(2));

        assertTrue(end instanceof SrtException,
            "expected the iteration to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a close-initiated cancel surfaces as CLOSED");
        assertEquals(RecvEndReason.CANCELLED, rx.endReason(),
            "close() cancels first, so the parked iteration recorded CANCELLED "
                + "and the close-time snapshot carries it");
        sender.close();
    }

    /**
     * Same contract on the basic-bytes shell: {@code close()} while
     * {@code recvBytes()} is parked on another thread ends that call with
     * {@code SrtException(CLOSED)} and returns.
     */
    @Test
    @Timeout(60)
    void closeWakesManagedReceiverParkedInRecvBytes() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedReceiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        CountDownLatch startReceiving = new CountDownLatch(1);

        Thread reader = new Thread(() -> {
            ManagedReceiver rx;
            try {
                rx = ManagedReceiver.fromUrl(listenUrl); // blocks until a peer connects
            } catch (Exception ex) {
                rxFuture.completeExceptionally(ex);
                endFuture.complete(ex);
                return;
            }
            rxFuture.complete(rx);
            try {
                startReceiving.await();
                for (;;) {
                    rx.recvBytes();
                }
            } catch (InterruptedException ie) {
                Thread.currentThread().interrupt();
                endFuture.complete(ie);
            } catch (RuntimeException | SrtException e) {
                endFuture.complete(e);
            }
        });
        reader.setDaemon(true);
        reader.start();

        ManagedMuxSender sender = connectSender(callerUrl, 5_000);
        ManagedReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        startReceiving.countDown();
        Thread.sleep(500);

        CompletableFuture<Void> closed = CompletableFuture.runAsync(rx::close);
        try {
            closed.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            rescue(sender, endFuture);
            fail("close() blocked for >5 s behind the parked recvBytes() instead of cancelling it");
            return;
        }

        Throwable end;
        try {
            end = endFuture.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            rescue(sender, endFuture);
            fail("close() returned but the parked recvBytes() did not end within 5 s");
            return;
        }
        reader.join(TimeUnit.SECONDS.toMillis(2));

        assertTrue(end instanceof SrtException,
            "expected recvBytes() to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a close-initiated cancel surfaces as CLOSED");
        sender.close();
    }

    /**
     * Unpark a reader the verdict has already failed: a frame from the
     * still-connected peer ends its native receive so the daemon thread can
     * exit. Best-effort — nothing here may mask the verdict.
     */
    private static void rescue(ManagedMuxSender sender, CompletableFuture<Throwable> endFuture) {
        try {
            sender.sendVideo(syntheticH264Idr(), 0L, true);
            endFuture.get(5, TimeUnit.SECONDS);
        } catch (Exception ignored) {
            // see above
        }
        try {
            sender.close();
        } catch (Exception ignored) {
            // see above
        }
    }
}
