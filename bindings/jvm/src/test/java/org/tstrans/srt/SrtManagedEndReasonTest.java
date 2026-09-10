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
 * Live-socket checks for {@link ManagedDemuxReceiver#endReason()}. Every test
 * here needs a real SRT link and is gated to Linux, like its
 * {@link SrtManagedListenerCancelTest} sibling; the socket-free managed-wrapper
 * checks live in {@link SrtManagedTest}.
 *
 * <p>{@link RecvEndReason} mirrors {@code tst_pipeline::RecvEndReason},
 * recorded first-writer-wins by {@code ManagedDemuxReceiver::recv_event} at
 * whichever site observed the terminal condition. Two of its three variants are
 * reachable from the managed-SRT path today (see {@code RecvEndReason}'s
 * javadoc): {@code RECONNECT_EXHAUSTED} when the reconnect budget runs out,
 * {@code CANCELLED} on a caller-fired cancel. {@code END_OF_STREAM} is not
 * produced here.
 */
class SrtManagedEndReasonTest {

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
     * A receiver whose stream has not ended reports no reason at all.
     *
     * <p>Guards the {@code -1} "nothing recorded" sentinel the native returns
     * and its mapping to Java {@code null} — note {@code 0} is a REAL ordinal
     * here ({@code END_OF_STREAM}), unlike {@code org.tstrans.rtp}'s enum where
     * the native ordinals start at 1.
     */
    @Test
    @Timeout(60)
    void endReasonIsNullOnALiveReceiver() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedDemuxReceiver> rxFuture = new CompletableFuture<>();
        Thread opener = new Thread(() -> {
            try {
                rxFuture.complete(ManagedDemuxReceiver.fromUrl(listenUrl));
            } catch (Exception ex) {
                rxFuture.completeExceptionally(ex);
            }
        });
        opener.setDaemon(true);
        opener.start();

        ManagedMuxSender sender = connectSender(callerUrl, 5_000);
        ManagedDemuxReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        try {
            assertNull(rx.endReason(), "a receiver that has not ended records no reason");
        } finally {
            Thread dropper = new Thread(sender::close);
            dropper.setDaemon(true);
            dropper.start();
            rx.close();
        }
    }

    /**
     * {@code endReason()} must answer while a native receive is in flight.
     *
     * <p>Same rider {@link SrtManagedListenerCancelTest} locks for
     * {@code cancelHandle()}: {@code nNext} holds the receiver's registry
     * resource lease for the whole duration of a native receive, so any getter
     * routed through {@code REGISTRY.with()} waits for a receive that — parked
     * in a listener-mode re-accept with no peer in sight — never returns. The
     * end-reason cell is captured at open and read off the registry entry
     * WITHOUT that lease, so this call must return promptly.
     *
     * <p>The reader is a daemon thread on purpose: a JUnit {@code @Timeout}
     * cannot interrupt a thread blocked in a native accept, so a regression is
     * rescued by cancelling rather than wedging the suite.
     */
    @Test
    @Timeout(60)
    void endReasonReturnsPromptlyWhileAReceiveIsParked() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedDemuxReceiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        Thread reader = new Thread(() -> {
            ManagedDemuxReceiver rx;
            try {
                rx = ManagedDemuxReceiver.fromUrl(listenUrl);
            } catch (Exception ex) {
                rxFuture.completeExceptionally(ex);
                endFuture.complete(ex);
                return;
            }
            rxFuture.complete(rx);
            try {
                for (DemuxEvent ignored : rx) {
                    // drain until the iteration ends
                }
                endFuture.complete(null);
            } catch (RuntimeException re) {
                endFuture.complete(re.getCause() != null ? re.getCause() : re);
            }
        });
        reader.setDaemon(true);
        reader.start();

        ManagedMuxSender sender = connectSender(callerUrl, 5_000);
        ManagedDemuxReceiver rx = rxFuture.get(5, TimeUnit.SECONDS);
        // Taken before the peer drop, so the daemon reader can always be freed.
        CancelHandle cancel = rx.cancelHandle();

        for (int i = 0; i < 5; i++) {
            sender.sendVideo(syntheticH264Idr(), i * 3000L, i == 0);
            Thread.sleep(10);
        }
        Thread.sleep(300);
        // Peer drop: the reader re-enters its factory (bind + accept) after the
        // default backoff and parks there with no peer in sight. Close on a side
        // daemon thread — libsrt's srt_close LINGERS.
        Thread dropper = new Thread(sender::close);
        dropper.setDaemon(true);
        dropper.start();
        Thread.sleep(1_000);

        // Off-thread so a regression fails the assertion instead of wedging main.
        CompletableFuture<RecvEndReason> reasonFuture = CompletableFuture.supplyAsync(
            rx::endReason, r -> {
                Thread t = new Thread(r, "end-reason-getter");
                t.setDaemon(true);
                t.start();
            });
        // The 2 s `get` IS the oracle: a blocked endReason() times out here. No
        // wall-clock "it was fast enough" assert follows — those are a known
        // flake class on loaded CI runners.
        try {
            reasonFuture.get(2, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            // Rescue, best-effort: free the parked reader so the suite can go on.
            // Any failure inside the rescue must not mask the verdict below.
            try {
                cancel.cancel();
                endFuture.get(5, TimeUnit.SECONDS);
            } catch (Exception ignored) {
                // see above
            }
            fail("endReason() blocked for >2 s behind a parked native receive");
            return;
        }
        cancel.cancel();
        endFuture.get(5, TimeUnit.SECONDS);
        reader.join(TimeUnit.SECONDS.toMillis(2));
        rx.close();
    }

    /**
     * A caller-initiated {@code cancel()} that ends a parked iteration records
     * {@link RecvEndReason#CANCELLED}.
     *
     * <p>The cancel handle is obtained BEFORE the reader iterates — the pattern
     * the class doc recommends and the one a caller can always rely on. The
     * iteration ends with {@code SrtException(CLOSED)}, which is the site
     * {@code ManagedDemuxReceiver::recv_event} records {@code Cancelled} at.
     */
    @Test
    @Timeout(60)
    void endReasonIsCancelledAfterCancelEndsTheIteration() throws Exception {
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
                rx = ManagedDemuxReceiver.fromUrl(listenUrl);
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
        CancelHandle cancel = rx.cancelHandle();
        startIterating.countDown();

        for (int i = 0; i < 5; i++) {
            sender.sendVideo(syntheticH264Idr(), i * 3000L, i == 0);
            Thread.sleep(10);
        }
        Thread.sleep(300);
        Thread dropper = new Thread(sender::close);
        dropper.setDaemon(true);
        dropper.start();
        Thread.sleep(1_000);

        cancel.cancel();
        Throwable end;
        try {
            end = endFuture.get(5, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            // Rescue: a new peer releases the accept so the daemon thread ends.
            try {
                ManagedMuxSender rescue = connectSender(callerUrl, 5_000);
                endFuture.get(5, TimeUnit.SECONDS);
                rescue.close();
            } catch (Exception ignored) {
                // best-effort; must not mask the verdict
            }
            fail("cancel() did not end the parked iteration within 5 s");
            return;
        }
        reader.join(TimeUnit.SECONDS.toMillis(2));

        assertTrue(end instanceof SrtException,
            "expected the iteration to end with SrtException(CLOSED), got " + end);
        assertEquals(RecvEndReason.CANCELLED, rx.endReason(),
            "a caller-initiated cancel records CANCELLED");
        rx.close();
        assertEquals(RecvEndReason.CANCELLED, rx.endReason(),
            "the close-time snapshot must preserve CANCELLED after close()");
    }

    /**
     * A peer that connects, sends, then leaves — against a zero-retry policy —
     * records {@link RecvEndReason#RECONNECT_EXHAUSTED}, and that value
     * survives {@code close()}.
     *
     * <p>Which variant fires is dictated by {@code tst-pipeline}: a peer FIN
     * surfaces as {@code TransportError::Broken}, which {@code
     * ManagedRecvTransport} treats as recoverable and retries. With {@code
     * maxAttempts(0)} the budget is exhausted on the first attempt, so the
     * decorator gives up with {@code TransportError::Closed} → shell {@code
     * EndOfStream} → {@code RecvEndReason::ReconnectExhausted}. {@code
     * END_OF_STREAM} is NOT produced by the managed-SRT path (see
     * {@code recv_end_reason.rs}).
     *
     * <p>The post-{@code close()} half is the {@code nClose} snapshot contract:
     * closing zeroes the Java handle and permanently removes the native
     * registry entry, so the reason has to be captured by {@code nClose} itself
     * and cached Java-side.
     */
    @Test
    @Timeout(60)
    void endReasonIsReconnectExhaustedWhenThePeerLeavesAndSurvivesClose() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;
        // Zero retries: the first post-drop reconnect attempt is already over
        // budget, so the decorator gives up instead of re-accepting.
        ReconnectPolicy noRetry = ReconnectPolicy.builder().maxAttempts(0).build();

        CompletableFuture<ManagedDemuxReceiver> rxFuture = new CompletableFuture<>();
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        CountDownLatch startIterating = new CountDownLatch(1);

        Thread reader = new Thread(() -> {
            ManagedDemuxReceiver rx;
            try {
                rx = ManagedDemuxReceiver.fromUrl(listenUrl, noRetry);
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
                endFuture.complete(null); // clean end: the budget ran out
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

        for (int i = 0; i < 5; i++) {
            sender.sendVideo(syntheticH264Idr(), i * 3000L, i == 0);
            Thread.sleep(10);
        }
        Thread.sleep(300);
        // Peer leaves for good. Close on a side daemon thread — srt_close LINGERS.
        Thread dropper = new Thread(sender::close);
        dropper.setDaemon(true);
        dropper.start();

        Throwable end = endFuture.get(15, TimeUnit.SECONDS);
        reader.join(TimeUnit.SECONDS.toMillis(2));
        assertNull(end, "a budget-exhausted managed receiver ends iteration cleanly, got " + end);

        assertEquals(RecvEndReason.RECONNECT_EXHAUSTED, rx.endReason(),
            "an exhausted reconnect budget records RECONNECT_EXHAUSTED");
        rx.close();
        assertEquals(RecvEndReason.RECONNECT_EXHAUSTED, rx.endReason(),
            "the close-time snapshot must preserve the reason after close()");
    }
}
