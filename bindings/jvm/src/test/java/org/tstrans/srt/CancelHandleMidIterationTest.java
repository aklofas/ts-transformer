package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;

/**
 * {@code cancelHandle()} must return promptly while another thread is parked in
 * {@code next()} on the same plain {@link DemuxReceiver}.
 *
 * <p>Sibling of the managed case in {@code SrtManagedListenerCancelTest}: the
 * native used to resolve the handle under the receiver's registry lease, which a
 * parked {@code nNext} holds, so the sanctioned cross-thread stop could only be
 * armed BEFORE iterating. The cancel target is now captured at open and read
 * without the lease.
 *
 * <p>Choreography: loopback listener + an idle caller-mode sender (connected, never
 * sends), so the reader's first {@code next()} parks indefinitely. Main asks for
 * the handle bounded and off-thread, then cancels and asserts the reader ends with
 * {@code SrtException(CLOSED)}. A watchdog handle obtained before iterating rescues
 * the reader if the call under test wedges, so a regression fails instead of hanging.
 */
final class CancelHandleMidIterationTest {
    private static final int LATENCY_MS = 120;

    @Test
    @Timeout(30)
    void cancelHandleWhileNextParkedReturnsPromptly() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();

        // Idle peer: connects and holds the link open without sending anything.
        CountDownLatch release = new CountDownLatch(1);
        Thread peer = new Thread(() -> {
            try (MuxSender tx = MuxSender.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                    roundtripConfig())) {
                release.await();
            } catch (Throwable ignored) {
                // Teardown races the receiver's close; benign.
            }
        }, "idle-peer");
        peer.setDaemon(true);
        peer.start();

        Socket sock = listener.accept(null);
        listener.close();
        try (DemuxReceiver rx = sock.intoDemuxReceiver()) {
            CancelHandle watchdog = rx.cancelHandle(); // rescue only, obtained pre-iteration

            CompletableFuture<Throwable> end = new CompletableFuture<>();
            CountDownLatch iterating = new CountDownLatch(1);
            Thread reader = new Thread(() -> {
                try {
                    iterating.countDown();
                    for (DemuxEvent ignored : rx) {
                        // nothing arrives; next() parks
                    }
                    end.complete(null);
                } catch (RuntimeException re) {
                    end.complete(re.getCause() != null ? re.getCause() : re);
                }
            }, "parked-reader");
            reader.setDaemon(true);
            reader.start();
            assertTrue(iterating.await(2, TimeUnit.SECONDS));
            Thread.sleep(300); // let next() actually park inside the native receive

            CompletableFuture<CancelHandle> handleFuture = CompletableFuture.supplyAsync(
                rx::cancelHandle, r -> {
                    Thread t = new Thread(r, "cancel-handle-getter");
                    t.setDaemon(true);
                    t.start();
                });
            CancelHandle cancel;
            long h0 = System.nanoTime();
            try {
                cancel = handleFuture.get(2, TimeUnit.SECONDS);
            } catch (TimeoutException te) {
                // Rescue, best-effort: the watchdog frees the lease so the getter
                // and reader can finish (the late getter may then see a closed
                // socket — irrelevant to the verdict, so swallow it).
                watchdog.cancel();
                try {
                    handleFuture.get(5, TimeUnit.SECONDS);
                } catch (Exception ignored) {
                    // see above
                }
                end.get(5, TimeUnit.SECONDS);
                fail("cancelHandle() blocked for >2 s behind a next() parked in receive");
                return;
            }
            long handleMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - h0);
            assertTrue(handleMs < 1_000,
                "cancelHandle() took " + handleMs + " ms while next() was parked");

            long t0 = System.nanoTime();
            cancel.cancel();
            Throwable cause = end.get(3, TimeUnit.SECONDS);
            long wokeMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertTrue(wokeMs < 2_000, "cancel took " + wokeMs + " ms to wake next()");
            assertTrue(cause instanceof SrtException, "expected an SrtException, got " + cause);
            // A plain (unmanaged) srt transport surfaces a cancel by closing the
            // socket under the parked recv, which maps to BROKEN; CLOSED is the
            // managed wrapper's mapping. Same acceptance set as the Python twin
            // (test_cancel_handle_cross_thread) and docs/reference/srt-cancel-handle.md.
            SrtException.Kind kind = ((SrtException) cause).kind();
            assertTrue(kind == SrtException.Kind.BROKEN || kind == SrtException.Kind.CLOSED,
                "cancel should surface as BROKEN or CLOSED, got " + kind);
        } finally {
            release.countDown();
        }
    }
}
