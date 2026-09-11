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
 * The cancel-first {@code close()} contract of the PLAIN srt receivers,
 * {@link DemuxReceiver} and {@link Receiver}: {@code close()} from another
 * thread wakes a parked receive instead of waiting behind it. Twin of
 * {@link SrtManagedCloseCancelsFirstTest}; parity with tst-py's plain
 * {@code DemuxReceiver.close()} / {@code Receiver.close()} and the C ABI's
 * {@code tst_demux_receiver_close}, which both cancel before tearing down.
 *
 * <p>The one difference from the managed pair is the kind the parked call ends
 * with: the plain cancel handle closes the libsrt socket, so the parked
 * {@code srt_recvmsg} fails with a connection error and surfaces as
 * {@code SrtException(BROKEN)} — the managed shells map their own cancel to
 * {@code CLOSED}. The plain shells record no end reason.
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
class SrtPlainCloseCancelsFirstTest {

    private static final int LATENCY_MS = 120;

    /** Connect a caller, retrying while the listener is between binds. */
    private static MuxSender connectSender(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return MuxSender.fromUrl(url, roundtripConfig());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /**
     * {@code close()} while {@code next()} is parked on another thread ends that
     * iteration with {@code SrtException(BROKEN)} and returns.
     */
    @Test
    @Timeout(60)
    void closeWakesParkedIteration() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<DemuxReceiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        CountDownLatch startIterating = new CountDownLatch(1);

        Thread reader = new Thread(() -> {
            DemuxReceiver rx;
            try {
                rx = DemuxReceiver.fromUrl(listenUrl); // blocks until a peer connects
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

        MuxSender sender = connectSender(callerUrl, 5_000);
        DemuxReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        startIterating.countDown();
        // Let the reader park inside the native receive: the peer is connected
        // and stays silent, so only a cancel can end that receive.
        Thread.sleep(500);

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
            "expected the iteration to end with SrtException(BROKEN), got " + end);
        assertEquals(SrtException.Kind.BROKEN, ((SrtException) end).kind(),
            "a close-initiated cancel on the plain shell surfaces as BROKEN");
        sender.close();
    }

    /**
     * Same contract on the basic-bytes shell: {@code close()} while
     * {@code recvBytes()} is parked on another thread ends that call with
     * {@code SrtException(BROKEN)} and returns.
     */
    @Test
    @Timeout(60)
    void closeWakesReceiverParkedInRecvBytes() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        CountDownLatch startReceiving = new CountDownLatch(1);

        Thread reader = new Thread(() -> {
            Receiver rx;
            try {
                rx = Receiver.fromUrl(listenUrl); // blocks until a peer connects
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

        MuxSender sender = connectSender(callerUrl, 5_000);
        Receiver rx = rxFuture.get(5, TimeUnit.SECONDS);
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
            "expected recvBytes() to end with SrtException(BROKEN), got " + end);
        assertEquals(SrtException.Kind.BROKEN, ((SrtException) end).kind(),
            "a close-initiated cancel on the plain shell surfaces as BROKEN");
        sender.close();
    }

    /**
     * Unpark a reader the verdict has already failed: a frame from the
     * still-connected peer ends its native receive so the daemon thread can
     * exit. Best-effort — nothing here may mask the verdict.
     */
    private static void rescue(MuxSender sender, CompletableFuture<Throwable> endFuture) {
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
