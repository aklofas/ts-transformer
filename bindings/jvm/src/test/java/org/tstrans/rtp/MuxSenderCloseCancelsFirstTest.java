package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtpException;

/**
 * The rtp {@link MuxSender} is an {@code Owned} entry since WP-B3: {@code close()}
 * from another thread cancels first, then frees. A {@code sendVideo} racing it
 * ends with {@code RtpException(CLOSED)} (the cancel landed mid-send) or
 * {@code IllegalStateException} (the handle was already claimed) — never a
 * different kind, a hang, or a torn free.
 *
 * <p>RTP/UDP sends do not park on loopback, so the cancel registration adds no
 * new wake-up here; this is a characterization pin on the close/send race that
 * the registration now has to survive (the sender registered NO cancel target
 * before — a plain {@code REGISTRY.insert}). Same shape as
 * {@link DemuxReceiverCloseRaceTest}'s memory-safety run.
 */
final class MuxSenderCloseCancelsFirstTest {
    @Test
    @Timeout(30)
    void closeVsSendIsCancelledOrClosed() throws Exception {
        for (int i = 0; i < 50; i++) {
            // A REAL bound peer socket, not a free port: RTP is fire-and-forget
            // UDP, and a loopback datagram to a port nobody holds comes back as
            // ICMP port-unreachable, which the next send surfaces as
            // RtpException(BROKEN) — a true transport failure that would mask
            // the close/send race this test is about.
            try (java.net.DatagramSocket peer = new java.net.DatagramSocket(0)) {
            MuxSender tx = MuxSender.fromUrl("rtp://127.0.0.1:" + peer.getLocalPort(), roundtripConfig());
            byte[] idr = syntheticH264Idr();
            CountDownLatch ready = new CountDownLatch(1);
            AtomicReference<Throwable> unexpected = new AtomicReference<>();
            Thread pump = new Thread(() -> {
                ready.countDown();
                long pts = 0;
                for (int k = 0; k < 2000; k++) {
                    try {
                        tx.sendVideo(idr, pts, true);
                        pts += 3_000;
                    } catch (RtpException e) {
                        if (e.kind() != RtpException.Kind.CLOSED) {
                            unexpected.set(e);
                        }
                        return;
                    } catch (IllegalStateException claimed) {
                        return; // handle claimed by close() — sanctioned
                    } catch (Throwable t) {
                        unexpected.set(t);
                        return;
                    }
                }
            }, "rtp-mux-pump-" + i);
            pump.setDaemon(true);
            pump.start();
            assertTrue(ready.await(2, TimeUnit.SECONDS));
            tx.close();
            pump.join(5_000);
            assertFalse(pump.isAlive(), "pump did not finish after close (run " + i + ")");
            assertNull(unexpected.get(), "pump saw an unexpected failure (run " + i + ")");
            }
        }
    }
}
