package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.cancelNow;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.Iterator;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.ShellPeers.Watchdog;

/**
 * A cancel is terminal for the managed demux receiver's iterator too: an event
 * {@code hasNext()} already holds is not served by a {@code next()} after
 * {@code cancelHandle().cancel()}. Topology as {@code LastSeenMicrosTest}: the
 * managed shell is the caller on this thread, a plain listener streams on a
 * daemon thread until the test is done.
 */
class ManagedDemuxCancelTerminalTest {

    private static final int LATENCY_MS = 120;
    private static final int TIMEOUT_SEC = 15;

    @Test
    @Timeout(TIMEOUT_SEC + 10)
    void cancelBetweenHasNextAndNextThrowsClosed() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");

        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        CountDownLatch done = new CountDownLatch(1);
        Thread peerThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            MuxSender ms = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().getPort());
                sock = listener.accept(null);
                ms = sock.intoMuxSender(roundtripConfig());
                long pts = 0;
                for (int round = 0; round < 400 && done.getCount() > 0; round++) {
                    for (int i = 0; i < 6; i++, pts += 3000L) {
                        ms.sendVideo(syntheticH264Idr(), pts, true);
                    }
                    Thread.sleep(50);
                }
            } catch (Exception ex) {
                portFuture.completeExceptionally(ex);
            } finally {
                if (ms != null) ms.close();
                if (sock != null) sock.close();
                if (listener != null) listener.close();
            }
        });
        peerThread.setDaemon(true);
        peerThread.start();
        int port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);

        try (ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS)) {
            // A no-hang safety net: a cancel handle taken before iterating.
            Watchdog watchdog = new Watchdog(rx.cancelHandle(), TimeUnit.SECONDS.toMillis(TIMEOUT_SEC - 3));
            try {
                Iterator<DemuxEvent> it = rx.iterator();
                assertTrue(it.hasNext(), "the stream must produce an event");
                cancelNow(rx::cancelHandle);
                RuntimeException e = assertThrows(RuntimeException.class, it::next,
                    "the event cached by hasNext() must not be served after cancel");
                SrtException se = assertInstanceOf(SrtException.class, e.getCause());
                assertEquals(SrtException.Kind.CLOSED, se.kind());
            } finally {
                done.countDown();
                watchdog.close();
            }
        }
        peerThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }
}
