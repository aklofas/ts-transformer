package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.PCR_LEAD_250_FIRST_PCR;
import static org.tstrans.TestSupport.firstPcrBase;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.pcrLead250Config;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.tstrans.shells.MuxSender;
import org.tstrans.shells.Receiver;

/**
 * A non-default {@code pcrLeadMs} reaches the muxer through the two SRT natives
 * that build one: {@link Socket#intoMuxSender} and {@link ManagedMuxSender#fromUrl}.
 * Each sender is the caller on the main thread; a plain listener on a daemon
 * thread reads the TS packets back and reports the first PCR on the wire.
 *
 * <p>Linux only and streamed until observed, like {@link SrtManagedLiveTest}.
 */
class SrtPcrLeadTest {

    private static final int TIMEOUT_SEC = 15;
    private static final int LATENCY_MS = 120;

    /** One video push at the given PTS. */
    @FunctionalInterface
    private interface VideoSend {
        void send(long pts) throws Exception;
    }

    /** The listening peer: its port, the first PCR it read, and the latch that releases its teardown. */
    private static final class Peer {
        final CompletableFuture<Integer> port = new CompletableFuture<>();
        final CompletableFuture<Long> firstPcr = new CompletableFuture<>();
        final CountDownLatch senderDone = new CountDownLatch(1);

        Peer() {
            Thread t = new Thread(() -> {
                Listener listener = null;
                Socket sock = null;
                Receiver rx = null;
                try {
                    listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                        .listener()
                        .listen();
                    port.complete(listener.localAddr().port());
                    sock = listener.accept(null);
                    rx = sock.intoReceiver();
                    long pcr = -1;
                    while (pcr < 0) {
                        pcr = firstPcrBase(rx.recvBytes()); // throws once the sender has gone
                    }
                    firstPcr.complete(pcr);
                } catch (Exception ex) {
                    port.completeExceptionally(ex);
                    firstPcr.completeExceptionally(ex);
                } finally {
                    // Hold teardown until the sender has stopped, so its last sends
                    // never land on a closing peer.
                    try {
                        senderDone.await(TIMEOUT_SEC, TimeUnit.SECONDS);
                    } catch (InterruptedException ignored) {
                        Thread.currentThread().interrupt();
                    }
                    if (rx != null) rx.close();
                    if (sock != null) sock.close();
                    if (listener != null) listener.close();
                }
            }, "pcr-reading-peer");
            t.setDaemon(true);
            t.start();
        }

        String callerUrl() throws Exception {
            return "srt://127.0.0.1:" + port.get(TIMEOUT_SEC, TimeUnit.SECONDS)
                + "?mode=caller&latency=" + LATENCY_MS;
        }

        /**
         * Push key frames from PTS 90 000 until the peer has read a PCR. A single
         * push followed by a close can be discarded before delivery; the first PCR
         * on the wire belongs to the first push either way. Bounded at about 10 s.
         */
        void streamUntilObserved(VideoSend send) throws Exception {
            long pts = 90_000L;
            for (int round = 0; round < 200 && !firstPcr.isDone(); round++) {
                for (int i = 0; i < 6; i++, pts += 3000L) {
                    send.send(pts);
                }
                Thread.sleep(50);
            }
        }
    }

    @Test
    void socketIntoMuxSenderPassesThePcrLead() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux");
        Peer peer = new Peer();
        Socket sock = new Builder(peer.callerUrl()).caller().connect();
        try (MuxSender s = sock.intoMuxSender(pcrLead250Config())) {
            peer.streamUntilObserved(pts -> s.sendVideo(syntheticH264Idr(), pts, true));
        } finally {
            peer.senderDone.countDown();
        }
        assertEquals(PCR_LEAD_250_FIRST_PCR, peer.firstPcr.get(TIMEOUT_SEC, TimeUnit.SECONDS));
    }

    @Test
    void managedMuxSenderPassesThePcrLead() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket loopback gated to Linux");
        Peer peer = new Peer();
        try (ManagedMuxSender s = ManagedMuxSender.fromUrl(peer.callerUrl(), pcrLead250Config())) {
            peer.streamUntilObserved(pts -> s.sendVideo(syntheticH264Idr(), pts, true));
        } finally {
            peer.senderDone.countDown();
        }
        assertEquals(PCR_LEAD_250_FIRST_PCR, peer.firstPcr.get(TIMEOUT_SEC, TimeUnit.SECONDS));
    }
}
