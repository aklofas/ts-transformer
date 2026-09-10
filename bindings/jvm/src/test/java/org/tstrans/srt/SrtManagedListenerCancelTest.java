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
 * {@code cancel()} must wake a listener-mode {@link ManagedDemuxReceiver} — or
 * its basic-bytes sibling {@link ManagedReceiver} — whose reconnect is parked in
 * the re-accept after its peer disconnected.
 *
 * <p>JVM mirror of tst-c's {@code loopback_cancel_wakes_managed_listener_parked_in_reaccept}
 * and tst-py's {@code test_cancel_wakes_managed_listener_parked_in_reaccept}
 * (ROADMAP "cancellable managed-listener re-accept"). Before the fix the
 * reconnect factory sat in {@code Listener::accept()} with nothing able to
 * reach that listener, and the backoff between attempts was an
 * uninterruptible sleep, so {@code cancel()} did nothing until the next peer
 * happened to connect.
 *
 * <p>Choreography:
 * <ol>
 *   <li>Reader daemon thread: {@code ManagedDemuxReceiver.fromUrl("srt://:P?mode=listener")}
 *       (blocks until a peer connects), then iterates until the iteration ends.</li>
 *   <li>Main: a {@link ManagedMuxSender} caller connects, pushes a few frames,
 *       then closes — the managed receiver sees the break and re-enters its
 *       factory (bind + accept) after the first backoff.</li>
 *   <li>Main: {@code cancelHandle().cancel()}; the reader must end within a
 *       couple of seconds with {@code SrtException(CLOSED)} (wrapped in the
 *       iterator's {@code RuntimeException}).</li>
 * </ol>
 *
 * <p>The reader is a daemon thread on purpose: a JUnit {@code @Timeout} cannot
 * interrupt a thread blocked in a native accept, so if the cancel does NOT
 * wake it a rescue peer is connected to release the accept, the thread is
 * joined, and the test fails with a clear message.
 *
 * <p>Two timings of {@code cancelHandle()} are covered. Obtained BEFORE the
 * reader iterates is the historical happy path. Obtained MID-ITERATION, while
 * the reader is parked in the re-accept, is the ROADMAP "JVM cancelHandle()
 * blocks while a native receive is in flight" rider: the native used to take
 * the receiver's registry lease, which {@code nNext} holds for the whole
 * duration of a native receive, so the call waited for a receive that — during
 * a re-accept — never returns. The cancel target is now captured at open and
 * read without that lease, so the call must return promptly.
 *
 * <p>{@link #cancelWakesManagedReceiverParkedInReaccept()} runs the same
 * choreography against {@link ManagedReceiver}, the basic-bytes wrapper over
 * {@code Receiver<ManagedRecvTransport<SrtTransport>>}. Its reconnect factory
 * shares the {@code FactoryCancel} slot with the demux twin, but that is a
 * separate JNI constructor and nothing pinned it: tst-py's matching wrapper was
 * found still on the plain, uncancellable factory (deep-review CORR-05), so the
 * JVM sibling gets its own regression lock here.
 */
class SrtManagedListenerCancelTest {
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

    @Test
    @Timeout(60) // safety net: a wedge fails instead of hanging the suite
    void cancelWakesManagedListenerParkedInReaccept() throws Exception {
        runCancelScenario(false);
    }

    @Test
    @Timeout(60)
    void cancelHandleObtainedWhileParkedInReacceptReturnsPromptly() throws Exception {
        runCancelScenario(true);
    }

    /**
     * @param obtainHandleMidIteration {@code false}: main takes the cancel handle
     *     before the reader iterates; {@code true}: the reader iterates at once and
     *     main takes the handle only after the reader is parked in the re-accept.
     */
    private void runCancelScenario(boolean obtainHandleMidIteration) throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedDemuxReceiver> rxFuture = new CompletableFuture<>();
        // How the iteration ended: the wrapped cause, or null for a clean end.
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        // Released by main once it holds the cancel handle (see the class doc).
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
                endFuture.complete(null); // clean end of iteration
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
        CancelHandle cancel = obtainHandleMidIteration ? null : rx.cancelHandle();
        startIterating.countDown();

        // A few frames so the link is genuinely up before the peer drops.
        for (int i = 0; i < 5; i++) {
            sender.sendVideo(syntheticH264Idr(), i * 3000L, i == 0);
            Thread.sleep(10);
        }
        Thread.sleep(300);
        // Peer drop: the managed receiver re-enters its factory (bind + accept)
        // after the default 100 ms backoff and parks there with no peer in sight.
        // Close on a side daemon thread: libsrt's srt_close LINGERS (same reason
        // SrtManagedReconnectTest drops its sender off-thread); the teardown
        // reaches the peer immediately either way.
        Thread dropper = new Thread(sender::close);
        dropper.setDaemon(true);
        dropper.start();
        Thread.sleep(1_000);

        if (obtainHandleMidIteration) {
            // The reader is now parked in the factory's re-accept with no peer in
            // sight. Ask for the handle from here, bounded: before the fix this
            // call blocked behind the reader's registry lease until a peer showed
            // up. Off-thread so a regression fails the assertion instead of
            // wedging main.
            CompletableFuture<CancelHandle> handleFuture = CompletableFuture.supplyAsync(
                rx::cancelHandle, r -> {
                    Thread t = new Thread(r, "cancel-handle-getter");
                    t.setDaemon(true);
                    t.start();
                });
            long h0 = System.nanoTime();
            try {
                cancel = handleFuture.get(2, TimeUnit.SECONDS);
            } catch (TimeoutException te) {
                // Rescue, best-effort: a new peer releases the accept, which frees
                // the lease the getter is queued on; cancel through whatever handle
                // we can get so the daemon reader ends. Any failure inside the
                // rescue must not mask the verdict below.
                try {
                    ManagedMuxSender rescue = connectSender(callerUrl, 5_000);
                    handleFuture.get(5, TimeUnit.SECONDS).cancel();
                    endFuture.get(5, TimeUnit.SECONDS);
                    rescue.close();
                } catch (Exception ignored) {
                    // see above
                }
                fail("cancelHandle() blocked for >2 s behind the reader parked in re-accept");
                return;
            }
            long handleMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - h0);
            assertTrue(handleMs < 1_000,
                "cancelHandle() took " + handleMs + " ms while a receive was in flight");
        }

        long t0 = System.nanoTime();
        cancel.cancel();

        Throwable end;
        try {
            end = endFuture.get(3, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            // Rescue: a new peer releases the accept so the daemon thread can be
            // joined; the cancel already latched, so the reader then exits.
            ManagedMuxSender rescue = connectSender(callerUrl, 5_000);
            endFuture.get(5, TimeUnit.SECONDS);
            rescue.close();
            fail("cancel() did not wake the managed listener parked in re-accept within 3 s");
            return;
        }
        long wokeMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
        reader.join(TimeUnit.SECONDS.toMillis(2));

        assertTrue(wokeMs < 2_000, "cancel took " + wokeMs + " ms to wake the parked re-accept");
        assertTrue(end instanceof SrtException,
            "expected the iteration to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a caller-initiated cancel surfaces as CLOSED");
        rx.close();
    }

    /**
     * The {@link ManagedReceiver} twin of
     * {@link #cancelWakesManagedListenerParkedInReaccept()}: a listener-mode
     * basic-bytes managed receiver parked in its reconnect re-accept must be
     * woken by {@code cancel()}.
     *
     * <p>The handle is taken BEFORE the reader starts receiving, which is the
     * pattern the class doc recommends and the one a caller can always rely on.
     * Same daemon-thread + rescue-peer shape as the demux twin, for the same
     * reason: a JUnit {@code @Timeout} cannot interrupt a blocked native accept.
     */
    @Test
    @Timeout(60) // safety net: a wedge fails instead of hanging the suite
    void cancelWakesManagedReceiverParkedInReaccept() throws Exception {
        assumeTrue(isLinux(),
            "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<ManagedReceiver> rxFuture = new CompletableFuture<>();
        // How the receive loop ended: the throwable that stopped it.
        CompletableFuture<Throwable> endFuture = new CompletableFuture<>();
        // Released by main once it holds the cancel handle (see the class doc).
        CountDownLatch startReceiving = new CountDownLatch(1);
        // Raised by the reader on its first delivered TS packet, so the peer drop
        // below happens against a link that has genuinely carried data.
        CountDownLatch firstPacket = new CountDownLatch(1);

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
                    firstPacket.countDown();
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
        // Obtained before any receive is in flight — the handle a caller keeps
        // for the whole session; it must survive every later reconnect.
        CancelHandle cancel = rx.cancelHandle();
        startReceiving.countDown();

        // Keep frames flowing until a packet has actually been delivered, rather
        // than sending a fixed few and hoping TSBPD released them in time (the
        // starved-receiver flake class SrtManagedLiveTest documents).
        for (int i = 0; i < 100 && firstPacket.getCount() > 0; i++) {
            sender.sendVideo(syntheticH264Idr(), i * 3000L, i == 0);
            Thread.sleep(20);
        }
        assertTrue(firstPacket.await(5, TimeUnit.SECONDS),
            "the managed receiver never delivered a packet before the peer drop");

        // Peer drop: the managed receiver re-enters its factory (bind + accept)
        // after the default 100 ms backoff and parks there with no peer in sight.
        // Close on a side daemon thread: libsrt's srt_close LINGERS.
        Thread dropper = new Thread(sender::close);
        dropper.setDaemon(true);
        dropper.start();
        Thread.sleep(1_000);

        long t0 = System.nanoTime();
        cancel.cancel();

        Throwable end;
        try {
            end = endFuture.get(3, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            // Rescue: a new peer releases the accept so the daemon thread can be
            // joined; the cancel already latched, so the reader then exits. Any
            // failure inside the rescue must not mask the verdict.
            try {
                ManagedMuxSender rescue = connectSender(callerUrl, 5_000);
                endFuture.get(5, TimeUnit.SECONDS);
                rescue.close();
            } catch (Exception ignored) {
                // see above
            }
            fail("cancel() did not wake the managed receiver parked in re-accept within 3 s");
            return;
        }
        long wokeMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
        reader.join(TimeUnit.SECONDS.toMillis(2));

        assertTrue(wokeMs < 2_000, "cancel took " + wokeMs + " ms to wake the parked re-accept");
        assertTrue(end instanceof SrtException,
            "expected the receive loop to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a caller-initiated cancel surfaces as CLOSED");
        rx.close();
    }
}
