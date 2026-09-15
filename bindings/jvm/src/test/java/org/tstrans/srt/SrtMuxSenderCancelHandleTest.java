package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.roundtripConfigWithData;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.srt.SrtSenderParkSupport.NON_READING_PEER_KNOBS;
import static org.tstrans.srt.SrtSenderParkSupport.awaitParked;
import static org.tstrans.srt.SrtSenderParkSupport.dataBlob;
import static org.tstrans.srt.SrtSenderParkSupport.managedParkPolicy;
import static org.tstrans.srt.SrtSenderParkSupport.pump;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicLong;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.srt.SrtSenderParkSupport.Pump;

/**
 * {@link MuxSender#cancelHandle()} / {@link ManagedMuxSender#cancelHandle()}:
 * the handle is obtainable from another thread WHILE a send is parked (the
 * target lives outside the registry's resource lock — same contract
 * {@link CancelHandleMidIterationTest} pins for the receiver), and
 * {@code cancel()} ends the parked send: {@code CLOSED} on the managed shell
 * (reconnect backoff), {@code BROKEN} on the plain shell (socket closed under
 * {@code srt_sendmsg}). Closes {@code A-ARCH-02}'s two JVM gaps.
 */
class SrtMuxSenderCancelHandleTest {

    private static final int LATENCY_MS = 120;

    private static CompletableFuture<Receiver> peerListener(String listenUrl) {
        CompletableFuture<Receiver> peerFuture = new CompletableFuture<>();
        Thread peer = new Thread(() -> {
            try {
                peerFuture.complete(Receiver.fromUrl(listenUrl));
            } catch (Exception ex) {
                peerFuture.completeExceptionally(ex);
            }
        }, "peer-listener");
        peer.setDaemon(true);
        peer.start();
        return peerFuture;
    }

    private static ManagedMuxSender connectManagedMux(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return ManagedMuxSender.fromUrl(url, roundtripConfig(), managedParkPolicy());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    private static MuxSender connectPlainMux(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return MuxSender.fromUrl(url, roundtripConfigWithData());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /** Obtain the handle on a daemon thread with a 2 s bound — lock-free means it never waits. */
    private static CancelHandle handleWhileParked(java.util.function.Supplier<CancelHandle> get)
            throws Exception {
        CompletableFuture<CancelHandle> f = CompletableFuture.supplyAsync(get, r -> {
            Thread t = new Thread(r, "cancel-handle-getter");
            t.setDaemon(true);
            t.start();
        });
        return f.get(2, TimeUnit.SECONDS);
    }

    @Test
    @Timeout(90)
    void managedCancelHandleEndsParkedSendVideoClosed() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300";

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        try (ManagedMuxSender sender = connectManagedMux(callerUrl, 5_000)) {
            Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);

            byte[] idr = syntheticH264Idr();
            AtomicLong pts = new AtomicLong();
            Pump pump = pump("parked-managed-mux-sender",
                () -> sender.sendVideo(idr, pts.getAndAdd(3_000), true), 10);
            Thread.sleep(300);
            peer.close();
            awaitParked(pump, "sendVideo");

            CancelHandle cancel;
            try {
                cancel = handleWhileParked(sender::cancelHandle);
            } catch (TimeoutException te) {
                try {
                    pump.end.get(30, TimeUnit.SECONDS); // budget unparks it
                } catch (Exception ignored) {
                    // see above
                }
                fail("cancelHandle() blocked for >2 s behind a sendVideo parked in the reconnect backoff");
                return;
            }
            long t0 = System.nanoTime();
            cancel.cancel();
            Throwable end = pump.end.get(3, TimeUnit.SECONDS);
            long wokeMs = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertTrue(end instanceof SrtException,
                "expected sendVideo to end with SrtException(CLOSED), got " + end);
            assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
                "cancel on the managed shell surfaces as CLOSED (woke after " + wokeMs + " ms)");
            assertTrue(cancel.isCancelled());
            cancel.close();
        }
    }

    @Test
    @Timeout(90)
    void plainCancelHandleEndsParkedSendDataBroken() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS
            + NON_READING_PEER_KNOBS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        try (MuxSender sender = connectPlainMux(callerUrl, 5_000)) {
            Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);

            byte[] blob = dataBlob();
            AtomicLong pts = new AtomicLong();
            Pump pump = pump("parked-plain-mux-sender",
                () -> sender.sendData(blob, pts.getAndAdd(3_000)), 0);
            awaitParked(pump, "sendData");

            CancelHandle cancel;
            try {
                cancel = handleWhileParked(sender::cancelHandle);
            } catch (TimeoutException te) {
                peer.close(); // rescue: the peer's shutdown breaks the parked srt_sendmsg
                try {
                    pump.end.get(5, TimeUnit.SECONDS);
                } catch (Exception ignored) {
                    // see above
                }
                fail("cancelHandle() blocked for >2 s behind a sendData parked in srt_sendmsg");
                return;
            }
            cancel.cancel();
            Throwable end = pump.end.get(3, TimeUnit.SECONDS);
            peer.close();
            assertTrue(end instanceof SrtException,
                "expected sendData to end with SrtException(BROKEN), got " + end);
            assertEquals(SrtException.Kind.BROKEN, ((SrtException) end).kind(),
                "cancel on the plain shell closes the socket under the parked send → BROKEN");
            cancel.close();
        }
    }

    @Test
    void cancelHandleAfterCloseThrowsIllegalState() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        MuxSender plain = connectPlainMux(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
        assertNotNull(plain.cancelHandle(), "open sender hands out a handle");
        plain.close();
        assertThrows(IllegalStateException.class, plain::cancelHandle);
        peer.close();
    }
}
