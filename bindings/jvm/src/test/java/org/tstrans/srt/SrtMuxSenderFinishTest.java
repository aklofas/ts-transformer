package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.srt.SrtSenderParkSupport.connectManagedMux;
import static org.tstrans.srt.SrtSenderParkSupport.connectPlainMux;
import static org.tstrans.srt.SrtSenderParkSupport.peerListener;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;

/**
 * Arc 2 R3 (DEBT-14): {@code finish()} drains, reports, closes; a second call
 * is quiet.
 *
 * <p>{@code close()} is cancel-first and abandons whatever the muxer still
 * holds; {@code finish()} is the lossless counterpart. The peer here is a
 * listener-mode {@link Receiver} that never reads, so libsrt's own send buffer
 * absorbs the payload and the drain has nothing to report — what these pin is
 * that {@code finish()} succeeds, leaves the sender closed, stays quiet on a
 * second call, and turns a later send into {@code SrtException(CLOSED)}.
 *
 * <p>Nothing parks, so no test needs a watchdog; every native call here
 * returns promptly, which is what keeps the {@code @Timeout} honest (a JUnit
 * {@code @Timeout} cannot interrupt a blocked native read).
 */
class SrtMuxSenderFinishTest {
    private static final int LATENCY_MS = 120;

    @Test
    @Timeout(60)
    void plainFinishClosesAndIsIdempotent() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux");
        int port = freeUdpPort();
        CompletableFuture<Receiver> peerFuture =
            peerListener("srt://:" + port + "?mode=listener&latency=" + LATENCY_MS);
        try (MuxSender sender = connectPlainMux(
                "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300", 5_000)) {
            Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
            sender.sendVideo(syntheticH264Idr(), 0, true);
            sender.finish();                 // nothing parked: drains + closes
            assertFalse(sender.isAlive());
            sender.finish();                 // quiet
            SrtException ex = assertThrows(SrtException.class,
                () -> sender.sendVideo(syntheticH264Idr(), 3_000, true));
            assertEquals(SrtException.Kind.CLOSED, ex.kind());
            peer.close();
        }
    }

    @Test
    @Timeout(60)
    void plainFinishAfterCloseIsQuiet() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux");
        int port = freeUdpPort();
        CompletableFuture<Receiver> peerFuture =
            peerListener("srt://:" + port + "?mode=listener&latency=" + LATENCY_MS);
        MuxSender sender = connectPlainMux(
            "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300", 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
        sender.close();
        sender.finish();                     // no handle left: quiet, not a throw
        peer.close();
    }

    @Test
    @Timeout(60)
    void managedFinishClosesAndIsIdempotent() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux");
        int port = freeUdpPort();
        CompletableFuture<Receiver> peerFuture =
            peerListener("srt://:" + port + "?mode=listener&latency=" + LATENCY_MS);
        try (ManagedMuxSender sender = connectManagedMux(
                "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300", 5_000)) {
            Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);
            sender.sendVideo(syntheticH264Idr(), 0, true);
            sender.finish();
            assertFalse(sender.isAlive());
            sender.finish();
            SrtException ex = assertThrows(SrtException.class,
                () -> sender.sendVideo(syntheticH264Idr(), 3_000, true));
            assertEquals(SrtException.Kind.CLOSED, ex.kind());
            peer.close();
        }
    }
}
