package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;

import java.util.concurrent.CountDownLatch;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.shells.MuxSender;

/**
 * A conversion that refuses its arguments leaves the {@link Socket} with the
 * caller: still open, still consumable exactly once. A null config used to
 * throw after the socket was claimed, leaving the native socket with no owner.
 *
 * <p>Loopback choreography as in {@link CancelHandleObservesCloseTest}, but the
 * accepted raw {@code Socket} is kept rather than converted.
 */
final class SocketConsumeValidationTest {
    private static final int LATENCY_MS = 120;

    /** The accepted socket + the latch that releases its idle peer. */
    private record Live(Socket sock, CountDownLatch release) implements AutoCloseable {
        @Override
        public void close() {
            release.countDown();
            sock.close(); // a no-op once a conversion consumed it
        }
    }

    private static Live connect() throws Exception {
        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();
        CountDownLatch release = new CountDownLatch(1);
        Thread peer = new Thread(() -> {
            try (MuxSender tx = MuxSender.open(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                    roundtripConfig())) {
                release.await();
            } catch (Throwable ignored) {
                // teardown races the socket's close; benign
            }
        }, "idle-peer");
        peer.setDaemon(true);
        peer.start();
        // Bounded: an unbounded native accept on the @Timeout thread cannot be
        // interrupted, so a failed peer connect would hang the suite.
        Socket sock = listener.accept(5000);
        listener.close();
        return new Live(sock, release);
    }

    @Test
    @Timeout(30)
    void nullDemuxConfigLeavesTheSocketWithTheCaller() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Live live = connect()) {
            assertThrows(NullPointerException.class,
                () -> live.sock.intoDemuxReceiver((DemuxerConfig) null));
            assertDoesNotThrow(() -> live.sock.peerAddr(), "the socket must still be open");
            live.sock.intoDemuxReceiver().close(); // still consumable exactly once
        }
    }

    @Test
    @Timeout(30)
    void nullMuxerConfigLeavesTheSocketWithTheCaller() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");
        try (Live live = connect()) {
            assertThrows(NullPointerException.class, () -> live.sock.intoMuxSender(null));
            assertDoesNotThrow(() -> live.sock.peerAddr());
        }
    }
}
