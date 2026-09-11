package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * Memory-safety stress test for {@link DemuxReceiver#close()} racing a concurrent native call.
 *
 * <p>Unlike an rtp {@code DemuxReceiver} (which binds a UDP socket and returns immediately), an
 * srt {@code DemuxReceiver} only exists once a peer has connected — its {@code fromUrl(listener)}
 * blocks in accept. So this test stands up a genuine loopback (listener accepts a caller-mode
 * sender that streams continuously) to obtain a live, connected receiver, then races
 * {@code close()} against (a) a cheap getter and (b) a parked {@code next()}. Linux-gated, like
 * the other srt live-socket tests.
 *
 * <p>The point under test is the leased {@code HandleRegistry}: a native call that races
 * {@code close()} either runs or throws a clean {@link IllegalStateException} — never UB. Case
 * (b) also pins the cancel-first {@code close()} contract ten times over: {@code close()} alone
 * must wake the parked {@code next()} (the single-shot kind assertion lives in
 * {@link SrtPlainCloseCancelsFirstTest}).
 */
final class DemuxReceiverCloseRaceTest {

    private static final int LATENCY_MS = 120;

    private static byte[] idr() {
        byte[] buf = new byte[20];
        buf[3] = 0x01;
        buf[4] = 0x65;
        for (int i = 0; i < 15; i++) {
            buf[5 + i] = (byte) (0xA5 ^ i);
        }
        return buf;
    }

    private static MuxerConfig cfg() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
    }

    /**
     * Stand up a loopback and hand back a CONNECTED {@link DemuxReceiver} (on this thread) plus a
     * daemon that keeps a caller-mode sender streaming so the receiver stays live. Returns the
     * receiver; the caller owns closing it. The sender daemon stops itself when {@code stop} flips.
     */
    private DemuxReceiver connectedReceiver(AtomicReference<Boolean> stop) throws Exception {
        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();

        // Continuous caller-mode sender on a daemon: connects, then streams muxed TS until stop.
        Thread sender = new Thread(() -> {
            try (MuxSender tx = MuxSender.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS, cfg())) {
                long pts = 0;
                while (!stop.get()) {
                    tx.sendVideo(idr(), pts, true);
                    pts += 3003;
                    Thread.sleep(5);
                }
            } catch (Throwable ignored) {
                // Sender teardown races receiver close; any error here is benign.
            }
        }, "race-sender");
        sender.setDaemon(true);
        sender.start();

        // Accept on this thread (blocks until the sender connects), then consume into a receiver.
        Socket sock = listener.accept(null);
        DemuxReceiver rx = sock.intoDemuxReceiver();
        listener.close(); // listener no longer needed once accepted
        return rx;
    }

    @Test
    @Timeout(30)
    void closeVsFreshEntryIsMemorySafe() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        for (int i = 0; i < 20; i++) {
            AtomicReference<Boolean> stop = new AtomicReference<>(false);
            DemuxReceiver rx = connectedReceiver(stop);

            CountDownLatch ready = new CountDownLatch(1);
            AtomicReference<Throwable> unexpected = new AtomicReference<>();

            Thread caller = new Thread(() -> {
                ready.countDown();
                for (int k = 0; k < 500; k++) {
                    try {
                        if (rx.isAlive()) {
                            rx.socketStats();
                        }
                    } catch (IllegalStateException expected) {
                        // Handle claimed by close() — sanctioned.
                    } catch (Throwable t) {
                        unexpected.set(t);
                        return;
                    }
                }
            }, "rx-caller-" + i);
            caller.start();

            assertTrue(ready.await(2, TimeUnit.SECONDS), "caller never started");
            rx.close();

            caller.join(5000);
            stop.set(true);
            assertFalse(caller.isAlive(), "caller did not finish (run " + i + ")");
            assertNull(unexpected.get(), "caller saw an unexpected failure (run " + i + ")");
        }
    }

    @Test
    @Timeout(30)
    void closeWhileNextParkedIsMemorySafe() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        for (int i = 0; i < 10; i++) {
            AtomicReference<Boolean> stop = new AtomicReference<>(false);
            DemuxReceiver rx = connectedReceiver(stop);

            // Pre-obtain a cancel handle for RESCUE ONLY: if close() regresses to waiting behind
            // the parked next(), the watchdog unparks the daemon reader after the verdict has
            // already failed. It is never the wake under test.
            CancelHandle watchdog = rx.cancelHandle();

            CompletableFuture<Throwable> iterResult = new CompletableFuture<>();
            CountDownLatch aboutToIterate = new CountDownLatch(1);

            Thread iter = new Thread(() -> {
                aboutToIterate.countDown();
                try {
                    for (DemuxEvent e : rx) {
                        assertNotNull(e);
                        // Stop the sender once data flows, so subsequent next() parks → the
                        // window close() must safely interrupt.
                        stop.set(true);
                    }
                    iterResult.complete(null);
                } catch (RuntimeException wrapped) {
                    iterResult.complete(null); // wrapped SrtException/DemuxException is fine
                } catch (Throwable t) {
                    iterResult.complete(t);
                }
            }, "rx-iter-" + i);
            iter.setDaemon(true);
            iter.start();

            assertTrue(aboutToIterate.await(2, TimeUnit.SECONDS), "iterator never started");
            // Give the iterator time to receive some events then park in next().
            Thread.sleep(300);

            // close() cancels first: the registry entry's close-time hook fires the receiver's
            // cancel target BEFORE taking the resource lock the parked next() holds, so close()
            // alone unwedges the iteration and returns. The bounded get is the assertion — a
            // regression to the old wait-behind-the-parked-recv contract fails here instead of
            // pinning the test until @Timeout.
            CompletableFuture<Void> closed = CompletableFuture.runAsync(rx::close);
            try {
                closed.get(8, TimeUnit.SECONDS);
            } catch (TimeoutException te) {
                watchdog.cancel();
                fail("close() blocked for >8 s behind the parked next() instead of cancelling it"
                    + " (run " + i + ")");
            }

            Throwable result = iterResult.get(8, TimeUnit.SECONDS);
            watchdog.close();
            stop.set(true);
            assertNull(result, "iterator saw an unexpected failure (run " + i + ")");
        }
    }
}
