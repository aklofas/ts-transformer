package org.tstrans.shells;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;

/** {@link DemuxReceiver#tsBytesDemuxed()} on a live TCP receiver. */
@Timeout(value = 60, threadMode = Timeout.ThreadMode.SEPARATE_THREAD)
class DemuxReceiverTsBytesDemuxedTest {

    /** Writes a clean stream, ends it, and drains the receiver to the end. */
    private static void feedAndDrain(DemuxReceiverTestSupport.Pair p) throws Exception {
        p.write(DemuxReceiverTestSupport.eventPackets());
        p.peer.shutdownOutput();
        List<DemuxEvent> events = new ArrayList<>();
        Object r = DemuxReceiverTestSupport.await(DemuxReceiverTestSupport.drainOnDaemon(p.rx, events));
        assertFalse(r instanceof Throwable, String.valueOf(r));
    }

    @Test
    void tsBytesDemuxedCountsWhatTheDemuxerWasFed() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            feedAndDrain(p);
            long n = p.rx.tsBytesDemuxed();
            assertEquals(DemuxReceiverTestSupport.eventPackets().length, n);
            assertEquals(0, n % 188);
        }
    }

    @Test
    void tsBytesDemuxedAnswersWhileNextIsParked() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            CompletableFuture<Object> parked = DemuxReceiverTestSupport.recvOnDaemon(p.rx);
            Thread.sleep(TestSupport.scaledMs(200));
            CompletableFuture<Long> answer = CompletableFuture.supplyAsync(p.rx::tsBytesDemuxed);
            assertEquals(0L, answer.get(TestSupport.scaledMs(2_000), TimeUnit.MILLISECONDS));
            assertFalse(parked.isDone(), "the receive stays parked");
            p.rescue.cancel();
            DemuxReceiverTestSupport.await(parked);
        }
    }

    @Test
    void tsBytesDemuxedReturnsTheLastValueAfterClose() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            feedAndDrain(p);
            long n = p.rx.tsBytesDemuxed();
            assertTrue(n > 0);
            p.rx.close();
            assertEquals(n, p.rx.tsBytesDemuxed());
            p.rx.close();
            assertEquals(n, p.rx.tsBytesDemuxed());
        }
    }

    @Test
    void tsBytesDemuxedNeverDecreasesWhileAnotherThreadCloses() throws Exception {
        try (DemuxReceiverTestSupport.Pair p = new DemuxReceiverTestSupport.Pair()) {
            feedAndDrain(p);
            long n = p.rx.tsBytesDemuxed();
            AtomicBoolean stop = new AtomicBoolean();
            CompletableFuture<Long> reader = CompletableFuture.supplyAsync(() -> {
                long prev = 0;
                while (!stop.get()) {
                    long v = p.rx.tsBytesDemuxed();
                    if (v < prev) throw new AssertionError("went from " + prev + " to " + v);
                    prev = v;
                }
                return prev;
            });
            Thread.sleep(TestSupport.scaledMs(20));
            p.rx.close();
            stop.set(true);
            assertEquals(n, reader.get(TestSupport.scaledMs(5_000), TimeUnit.MILLISECONDS));
            assertEquals(n, p.rx.tsBytesDemuxed());
        }
    }
}
