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

/**
 * {@code ManagedReceiver.reconnectAttempts()} (WP-B3 / ARCH-08): counts
 * reconnect ATTEMPTS (factory invocations, {@code ManagedHandles.attempts})
 * and answers WITHOUT the resource lock — while the receive loop is parked
 * in a listener-mode re-accept with no peer in sight. Before B3 it returned
 * the SUCCESS counter and took the lock a parked {@code recvBytes()} holds.
 *
 * <p>Attempts and reconnects are deliberately distinguished: the parked
 * re-accept is attempt #1 with ZERO successful rebuilds, so a getter still
 * wired to the success counter reads 0 here and fails.
 *
 * <p>Choreography: a peer connects, streams one frame so {@code recvBytes()}
 * returns, then closes; the managed receiver observes Broken, invokes the
 * factory (attempt #1) and parks in the re-accept. The reader is a daemon;
 * the pre-obtained {@link CancelHandle} is the rescue on every path.
 */
final class ManagedReceiverAttemptsLockFreeTest {
    private static final int LATENCY_MS = 120;

    private static ReconnectPolicy policy() {
        return ReconnectPolicy.builder()
            .backoff(BackoffStrategy.constant(50))
            .mode(ReconnectMode.BLOCKING)
            .build(); // maxAttempts unset → retry forever; the cancel handle is the exit
    }

    @Test
    @Timeout(60)
    void attemptsCountFactoryCallsAndReadLockFreeWhileReaccepting() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedReceiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        CountDownLatch gotFirst = new CountDownLatch(1);
        Thread reader = new Thread(() -> {
            ManagedReceiver rx;
            try {
                rx = ManagedReceiver.fromUrl(listenUrl, policy()); // blocks until a peer connects
            } catch (Exception ex) {
                rxFuture.completeExceptionally(ex);
                endFuture.complete(ex);
                return;
            }
            rxFuture.complete(rx);
            try {
                rx.recvBytes();          // the peer's frame
                gotFirst.countDown();
                for (;;) {
                    rx.recvBytes();      // peer gone → Broken → factory → parked re-accept
                }
            } catch (RuntimeException | SrtException e) {
                endFuture.complete(e);
            }
        }, "managed-reader");
        reader.setDaemon(true);
        reader.start();

        MuxSender peer = null;
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        SrtException last = null;
        while (peer == null && System.nanoTime() < deadline) {
            try {
                peer = MuxSender.fromUrl(callerUrl, roundtripConfig());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        assertNotNull(peer, "caller could not connect: " + last);
        ManagedReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        CancelHandle rescue = rx.cancelHandle(); // pre-obtained: the only thing that ends a parked re-accept
        try {
            // Enough frames to cross the sender's 1316-byte bundle threshold AND
            // the Receiver syncer's 4-packet sync window — this class exposes no
            // flush(), so the bytes only leave on a full bundle.
            byte[] idr = syntheticH264Idr();
            for (int i = 0; i < 16; i++) {
                peer.sendVideo(idr, i * 3_000L, i == 0);
            }
            assertTrue(gotFirst.await(5, TimeUnit.SECONDS), "the first frame never arrived");
            peer.close();                          // the link breaks; the loop re-binds and parks in accept
            Thread.sleep(1_000);                   // let Broken → factory → re-accept happen

            // The call under test, bounded and off-thread: lock-free means it answers
            // now, while recvBytes() is parked inside the re-accept.
            CompletableFuture<Long> attempts = CompletableFuture.supplyAsync(rx::reconnectAttempts, r -> {
                Thread t = new Thread(r, "attempts-getter");
                t.setDaemon(true);
                t.start();
            });
            long n;
            try {
                n = attempts.get(2, TimeUnit.SECONDS);
            } catch (TimeoutException te) {
                fail("reconnectAttempts() blocked for >2 s behind the parked re-accept");
                return;
            }
            // ATTEMPTS, not successes: the factory is parked in a re-accept that has
            // not completed, so the success counter is still 0 here.
            assertTrue(n >= 1, "one factory invocation happened (the parked re-accept); got " + n);
        } finally {
            rescue.cancel();                       // ends the parked re-accept on every path
            rx.close();                            // never leak a live receiver (R-EXIT class)
        }
        // Terminal-kind verdicts live AFTER the body, not in `finally`: a
        // failure here would otherwise mask the body's own failure.
        Throwable end = endFuture.get(10, TimeUnit.SECONDS);
        assertTrue(end instanceof SrtException, "expected the loop to end with SrtException, got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a caller-initiated cancel on the managed shell surfaces as CLOSED");
    }
}
