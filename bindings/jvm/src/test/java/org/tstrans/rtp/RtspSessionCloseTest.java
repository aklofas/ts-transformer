package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;

import java.util.Optional;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;
import org.tstrans.RtspException;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.EndReason;

/**
 * How an RTSP client session ends: {@code close()} (silent, bounded by the close's
 * TEARDOWN and thread waits) against {@code teardown()} (reports, waits up to the request
 * timeout), what a live TCP-interleaved receiver sees after each, and what a second thread
 * may do while a control call is parked. Every peer is a {@link RawRtspPeer} whose
 * TEARDOWN and PAUSE replies the test chooses.
 */
class RtspSessionCloseTest {

    /**
     * Above the close's own bounds (500 ms for the TEARDOWN, up to 2 s for the client's
     * thread) with room for a loaded host, and well under the 10 s request timeout the
     * close used to wait for.
     */
    private static final long CLOSE_BOUND_MS = 5_000;

    // ── (i) a refused TEARDOWN ───────────────────────────────────────────────

    @Test
    @Timeout(30)
    void closeAfterARefusedTeardownIsSilentAndEndsTheReceiverClosed() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, true, 200, 403);
             RtspSession session = RtspClient.connect(peer.config());
             Drain drain = new Drain(session.intoDemuxReceiver())) {
            drain.awaitLive();
            // A receiver taken from an RTSP session reaches its RTP counters.
            assertTrue(drain.rx.rtpStats().isPresent());
            session.close();
            assertTrue(peer.awaitTeardown(CLOSE_BOUND_MS), "close() sent TEARDOWN");
            assertClosed(drain.awaitEnd(CLOSE_BOUND_MS));
            assertEquals(Optional.of(EndReason.CANCELLED), drain.rx.endReason());
            drain.rx.close();
            assertTrue(drain.rx.rtpStats().isPresent());
        }
    }

    @Test
    @Timeout(30)
    void teardownReportsARefusedTeardown() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, 200, 403);
             RtspSession session = RtspClient.connect(peer.config())) {
            RtspException e = assertThrows(RtspException.class, session::teardown);
            assertEquals(RtspException.Kind.PROTOCOL, e.kind());
            assertTrue(session.isTornDown(), "a refused TEARDOWN still counts as torn down");
        }
    }

    // ── (ii) an unanswered TEARDOWN ──────────────────────────────────────────

    @Test
    @Timeout(30)
    void closeOnAnUnansweredTeardownReturnsWithinItsBound() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, true, 200, null);
             RtspSession session = RtspClient.connect(peer.config());
             Drain drain = new Drain(session.intoDemuxReceiver())) {
            drain.awaitLive();
            long t0 = System.nanoTime();
            session.close();
            long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertTrue(ms < CLOSE_BOUND_MS, "close() took " + ms + " ms");
            assertTrue(peer.awaitTeardown(CLOSE_BOUND_MS), "close() sent TEARDOWN");
            assertClosed(drain.awaitEnd(CLOSE_BOUND_MS));
        }
    }

    // ── (iii) a second thread while pause() is parked ───────────────────────

    @Test
    @Timeout(60)
    void aSecondThreadTakesTheCancelHandleAndClosesWhilePauseIsParked() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, null, 200)) {
            RtspSession session = RtspClient.connect(peer.config());
            Worker<Void> pause = new Worker<>("parked-pause", () -> {
                session.pause();
                return null;
            });
            Worker<RtspCancelHandle> taker = null;
            Worker<Void> closer = null;
            RtspCancelHandle handle = null;
            try {
                assertTrue(peer.awaitPause(10_000), "the peer never read PAUSE");
                taker = new Worker<>("cancel-handle", session::cancelHandle);
                handle = taker.get(2_000, "cancelHandle() while pause() is parked");
                assertFalse(handle.isCancelled());

                closer = new Worker<>("closer", () -> {
                    session.close();
                    return null;
                });
                Throwable t = pause.failure(CLOSE_BOUND_MS, "the parked pause()");
                RtspException e = assertInstanceOf(RtspException.class, t);
                assertEquals(RtspException.Kind.CLOSED, e.kind());
                closer.get(CLOSE_BOUND_MS, "close() while pause() is parked");
                assertTrue(handle.isCancelled(), "a handle taken before close() reads cancelled");
            } finally {
                session.close();
                pause.join(15_000);
                if (taker != null) taker.join(15_000);
                if (closer != null) closer.join(15_000);
                if (handle != null) handle.close();
            }
        }
    }

    // ── (iv) cancelHandle() after teardown() and after close() ──────────────

    @Test
    @Timeout(30)
    void cancelHandleAfterTeardownAndAfterClose() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, 200, 200);
             RtspSession session = RtspClient.connect(peer.config());
             RtspCancelHandle before = session.cancelHandle()) {
            session.teardown();
            try (RtspCancelHandle after = session.cancelHandle()) {
                assertFalse(after.isCancelled(), "teardown() does not cancel");
                session.close();
                assertThrows(IllegalStateException.class, session::cancelHandle);
                assertTrue(before.isCancelled(), "close() ends the client: its cancel is set");
                assertTrue(after.isCancelled());
            }
        }
    }

    // ── (v) close() on a peer that accepts the TEARDOWN ─────────────────────

    /**
     * An idle {@code close()} writes its TEARDOWN before anything sets the cancel, so a
     * TEARDOWN the server accepts can still give a live receiver end of stream. Checked at
     * the peer, when it reads the TEARDOWN, so it does not depend on the close's 500 ms
     * reply bound.
     */
    @Test
    @Timeout(30)
    void anIdleCloseWritesItsTeardownBeforeAnyCancel() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, 200, 200);
             RtspSession session = RtspClient.connect(peer.config());
             RtspCancelHandle handle = session.cancelHandle()) {
            peer.watchCancelAtTeardown(handle);
            session.close();
            assertTrue(peer.awaitTeardown(CLOSE_BOUND_MS), "close() sent TEARDOWN");
            assertEquals(Boolean.FALSE, peer.cancelledAtTeardown.get(),
                "the session was cancelled before its TEARDOWN was written");
            assertTrue(handle.isCancelled(), "close() ends the client: its cancel is set");
        }
    }

    @Test
    @Timeout(30)
    void closeOnAPeerThatAcceptsTheTeardownEndsTheReceiver() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, true, 200, 200);
             RtspSession session = RtspClient.connect(peer.config());
             Drain drain = new Drain(session.intoDemuxReceiver())) {
            drain.awaitLive();
            session.close();
            // End of stream when the 200 arrived inside close()'s 500 ms bound, CLOSED
            // when a loaded host delayed it past the bound: the test pins that the
            // receiver ends, not which (teardown() first is the way to wait for the reply).
            Throwable t = drain.awaitEnd(CLOSE_BOUND_MS);
            if (t != null) assertClosed(t);
        }
    }

    // ── (vi) a failed intoH264Receiver() ────────────────────────────────────

    @Test
    @Timeout(30)
    void aFailedIntoH264ReceiverOnAnMp2tSessionReturnsWithinTheCloseBound() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, 200, null);
             RtspSession session = RtspClient.connect(peer.config())) {
            long t0 = System.nanoTime();
            RtspException e = assertThrows(RtspException.class, session::intoH264Receiver);
            long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertEquals(RtspException.Kind.PROTOCOL, e.kind());
            assertTrue(ms < CLOSE_BOUND_MS, "intoH264Receiver() took " + ms + " ms");
            assertTrue(peer.awaitTeardown(CLOSE_BOUND_MS), "the failed call sent TEARDOWN");
        }
    }

    @Test
    @Timeout(30)
    void aFailedIntoH264ReceiverAfterIntoDemuxReceiverEndsTheReceiverClosed() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, true, 200, null);
             RtspSession session = RtspClient.connect(peer.config());
             Drain drain = new Drain(session.intoDemuxReceiver())) {
            drain.awaitLive();
            long t0 = System.nanoTime();
            RtspException e = assertThrows(RtspException.class, session::intoH264Receiver);
            long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertEquals(RtspException.Kind.CLOSED, e.kind());
            assertTrue(ms < CLOSE_BOUND_MS, "intoH264Receiver() took " + ms + " ms");
            assertClosed(drain.awaitEnd(CLOSE_BOUND_MS));
        }
    }

    // ── an H264Receiver made from an RTSP session ───────────────────────────

    @Test
    @Timeout(30)
    void anRtspMadeH264ReceiverCloseOnAnUnansweredTeardownIsBounded() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.H264_SDP, false, 200, null);
             RtspSession session = RtspClient.connectH264(peer.config());
             H264Receiver rx = session.intoH264Receiver()) {
            long t0 = System.nanoTime();
            rx.close();
            long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertTrue(ms < CLOSE_BOUND_MS, "close() took " + ms + " ms");
            assertTrue(peer.awaitTeardown(CLOSE_BOUND_MS), "close() sent TEARDOWN");
            assertEquals(Optional.of(EndReason.CANCELLED), rx.endReason());
        }
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    private static void assertClosed(Throwable t) {
        RtpException e = assertInstanceOf(RtpException.class, t, "the receiver ended with " + t);
        assertEquals(RtpException.Kind.CLOSED, e.kind());
    }

    /** A {@link DemuxReceiver} drained on a daemon thread until it ends. */
    private static final class Drain implements AutoCloseable {
        final DemuxReceiver rx;
        private final CountDownLatch firstEvent = new CountDownLatch(1);
        /** {@code null} for end of stream, else what the receive threw. */
        private final CompletableFuture<Throwable> end = new CompletableFuture<>();
        private final CancelHandle rescue;
        private final Thread thread;

        Drain(DemuxReceiver rx) {
            this.rx = rx;
            this.rescue = rx.cancelHandle(); // cancel-handle: closed in close()
            this.thread = new Thread(() -> {
                try {
                    while (rx.recvEvent() != null) firstEvent.countDown();
                    end.complete(null);
                } catch (Throwable t) {
                    end.complete(t);
                }
            }, "rtsp-close-drain");
            thread.setDaemon(true);
            thread.start();
        }

        void awaitLive() throws InterruptedException {
            assertTrue(firstEvent.await(10, TimeUnit.SECONDS), "the receiver delivered no event");
        }

        /**
         * What ended the receive ({@code null} = end of stream). Fails if it is still
         * running after {@code ms}, and cancels it then so the test cannot hang.
         */
        Throwable awaitEnd(long ms) throws Exception {
            try {
                return end.get(ms, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                rescue.cancel();
                throw new AssertionError("the receiver did not end within " + ms + " ms");
            }
        }

        @Override public void close() throws Exception {
            rescue.cancel();
            thread.join(5_000);
            rescue.close();
            rx.close();
            if (thread.isAlive()) throw new AssertionError("the drain thread did not end");
        }
    }

    /** A call on a daemon thread. */
    private static final class Worker<T> {
        private final CompletableFuture<T> result = new CompletableFuture<>();
        private final Thread thread;

        Worker(String name, Callable<T> call) {
            thread = new Thread(() -> {
                try {
                    result.complete(call.call());
                } catch (Throwable t) {
                    result.completeExceptionally(t);
                }
            }, name);
            thread.setDaemon(true);
            thread.start();
        }

        /** The call's value; fails if it threw or did not return within {@code ms}. */
        T get(long ms, String what) throws Exception {
            try {
                return result.get(ms, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                throw new AssertionError(what + " did not return within " + ms + " ms");
            } catch (ExecutionException e) {
                throw new AssertionError(what + " threw " + e.getCause(), e.getCause());
            }
        }

        /** What the call threw; fails if it returned or did not end within {@code ms}. */
        Throwable failure(long ms, String what) throws Exception {
            try {
                result.get(ms, TimeUnit.MILLISECONDS);
            } catch (TimeoutException e) {
                throw new AssertionError(what + " did not end within " + ms + " ms");
            } catch (ExecutionException e) {
                return e.getCause();
            }
            throw new AssertionError(what + " returned normally");
        }

        void join(long ms) throws InterruptedException {
            thread.join(ms);
            if (thread.isAlive()) throw new AssertionError(thread.getName() + " is still running");
        }
    }
}
