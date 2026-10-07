package org.tstrans.hls;

import static org.junit.jupiter.api.Assertions.*;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;

/**
 * Two-thread contracts of {@link HlsPublisher}: the construction-constant
 * getters answer while a push is in flight, and {@code close()} racing a push
 * is memory-safe. No wall-clock duration assertions: a latch proves the
 * overlap, {@code @Timeout} is the only clock and it fails the test, not a
 * numeric bound.
 */
class HlsCrossThreadTest {
    static byte[] bigChunk() {
        byte[] b = new byte[188 * 64];
        for (int i = 0; i < 64; i++) b[i * 188] = 0x47;
        return b;
    }

    /** Pusher loop on a daemon thread; counts down `started` after its first push. */
    static Thread pusher(HlsPublisher pub, AtomicBoolean stop, CountDownLatch started, AtomicReference<Throwable> failed) {
        Thread t = new Thread(() -> {
            try {
                byte[] chunk = bigChunk();
                while (!stop.get()) {
                    pub.pushTs(chunk);
                    started.countDown();
                }
            } catch (IllegalStateException closed) {
                started.countDown(); // sanctioned: close() claimed the handle
            } catch (Throwable t2) {
                failed.set(t2);
                started.countDown();
            }
        }, "hls-pusher");
        t.setDaemon(true);
        t.start();
        return t;
    }

    @Test
    @Timeout(60)
    void gettersAnswerWhileAnotherThreadPushes(@TempDir Path dir) throws Exception {
        HlsPublisher pub = HlsPublisher.builder().bind("127.0.0.1:0").outputDir(dir.toString()).build();
        AtomicBoolean stop = new AtomicBoolean(false);
        CountDownLatch started = new CountDownLatch(1);
        AtomicReference<Throwable> failed = new AtomicReference<>();
        Thread t = pusher(pub, stop, started, failed);
        assertTrue(started.await(20, TimeUnit.SECONDS), "the pusher never completed a push");
        try {
            for (int i = 0; i < 2000; i++) {
                assertTrue(pub.localAddr().isPresent());
                assertTrue(pub.localPort() > 0);
                assertEquals("HlsPublisher(open)", pub.toString());
                pub.stats();
                pub.hlsStats();
                pub.renderPlaylist(false);
            }
        } finally {
            stop.set(true);
            t.join(20_000);
        }
        assertFalse(t.isAlive(), "the pusher did not stop");
        assertNull(failed.get(), String.valueOf(failed.get()));
        pub.close();
    }

    @Test
    @Timeout(120)
    void closeRacingPushIsMemorySafe(@TempDir Path dir) throws Exception {
        for (int round = 0; round < 20; round++) {
            Path roundDir = dir.resolve("r" + round);
            Files.createDirectories(roundDir);
            HlsPublisher pub = HlsPublisher.builder().bind("127.0.0.1:0")
                .outputDir(roundDir.toString()).build();
            AtomicBoolean stop = new AtomicBoolean(false);
            CountDownLatch started = new CountDownLatch(1);
            AtomicReference<Throwable> failed = new AtomicReference<>();
            Thread t = pusher(pub, stop, started, failed);
            assertTrue(started.await(20, TimeUnit.SECONDS), "the pusher never completed a push");
            pub.close();            // races the pusher's next pushTs
            stop.set(true);
            t.join(20_000);
            assertFalse(t.isAlive(), "pusher wedged after close in round " + round);
            assertNull(failed.get(), "round " + round + ": " + failed.get());
            assertThrows(IllegalStateException.class, pub::stats);
        }
    }
}
