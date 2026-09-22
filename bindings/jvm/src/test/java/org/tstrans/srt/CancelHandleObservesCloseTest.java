package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;

/**
 * {@code CancelHandle.isCancelled()} reads the shell's ONE cancel state
 * (WP-B3, {@code Owned::is_cancelled}): it flips when ANY handle on the
 * shell cancels and when the shell is {@code close()}d (close cancels
 * first). Before B3 each handle carried its own flag, so a second handle
 * stayed {@code false} and {@code close()} set nothing.
 *
 * <p>Loopback choreography as in {@link CancelHandleMidIterationTest}: an
 * idle caller peer keeps the link open, the reader parks in {@code next()}
 * on a daemon thread, and a pre-obtained handle is the rescue on every
 * failure path so a regression fails instead of hanging.
 */
final class CancelHandleObservesCloseTest {
    private static final int LATENCY_MS = 120;

    /** A connected plain DemuxReceiver + the latch that releases its idle peer. */
    private static final class Live {
        final DemuxReceiver rx;
        final CountDownLatch release;

        Live(DemuxReceiver rx, CountDownLatch release) {
            this.rx = rx;
            this.release = release;
        }
    }

    private static Live connect() throws Exception {
        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();
        CountDownLatch release = new CountDownLatch(1);
        Thread peer = new Thread(() -> {
            try (MuxSender tx = MuxSender.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                    roundtripConfig())) {
                release.await();
            } catch (Throwable ignored) {
                // teardown races the receiver's close; benign
            }
        }, "idle-peer");
        peer.setDaemon(true);
        peer.start();
        Socket sock = listener.accept(null);
        listener.close();
        return new Live(sock.intoDemuxReceiver(), release);
    }

    /** Park a daemon reader in {@code next()}; the future completes with the ending throwable. */
    private static CompletableFuture<Throwable> parkReader(DemuxReceiver rx) throws Exception {
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
        return end;
    }

    @Test
    @Timeout(30)
    void secondHandleObservesCancelFromFirst() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        Live live = connect();
        try (DemuxReceiver rx = live.rx) {
            CancelHandle a = rx.cancelHandle();
            CancelHandle b = rx.cancelHandle();
            // A THIRD handle is the rescue. It must not be `b`: cancelling the
            // handle under test would set that handle's own flag and the verdict
            // would hold under the old per-handle model too (a vacuous pass).
            CancelHandle rescue = rx.cancelHandle();
            assertFalse(a.isCancelled());
            assertFalse(b.isCancelled());
            CompletableFuture<Throwable> end = parkReader(rx);
            a.cancel();
            final Throwable cause;
            final boolean secondHandleSaw;
            final boolean cancellerSaw;
            try {
                cause = end.get(10, TimeUnit.SECONDS);
                // Read both verdicts while nothing but `a.cancel()` has run.
                cancellerSaw = a.isCancelled();
                secondHandleSaw = b.isCancelled();
            } finally {
                rescue.cancel(); // frees the reader on a failed verdict; never asserted on
            }
            assertTrue(cause instanceof SrtException, "expected an SrtException, got " + cause);
            // WP-C2 tightens to CLOSED: today the plain srt cancel closes the socket
            // under the parked recv, which surfaces as BROKEN (PR #209).
            SrtException.Kind kind = ((SrtException) cause).kind();
            assertTrue(kind == SrtException.Kind.BROKEN || kind == SrtException.Kind.CLOSED,
                "cancel surfaces as BROKEN (CLOSED after WP-C2), got " + kind);
            assertTrue(cancellerSaw, "the cancelling handle observes its own cancel");
            assertTrue(secondHandleSaw,
                "a second handle on the same shell observes the cancel (one state)");
        } finally {
            live.release.countDown();
        }
    }

    @Test
    @Timeout(30)
    void closeIsObservedAsCancelled() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        Live live = connect();
        try {
            CancelHandle handle = live.rx.cancelHandle();
            // Separate rescue handle, obtained while the receiver is still open
            // (cancelHandle() throws once it is closed) — cancelling `handle`
            // itself would set the very flag under test.
            CancelHandle rescue = live.rx.cancelHandle();
            assertFalse(handle.isCancelled());
            CompletableFuture<Throwable> end = parkReader(live.rx);
            CompletableFuture<Void> closed = CompletableFuture.runAsync(live.rx::close);
            final boolean sawClose;
            try {
                closed.get(10, TimeUnit.SECONDS);
                assertNotNull(end.get(10, TimeUnit.SECONDS), "close() cancels first, so the parked next() ends");
                sawClose = handle.isCancelled();
            } finally {
                rescue.cancel(); // rescue; never asserted on
            }
            assertTrue(sawClose, "close() cancels first; a handle obtained before it observes that");
            assertThrows(IllegalStateException.class, live.rx::cancelHandle, "closed receiver");
        } finally {
            live.release.countDown();
        }
    }
}
