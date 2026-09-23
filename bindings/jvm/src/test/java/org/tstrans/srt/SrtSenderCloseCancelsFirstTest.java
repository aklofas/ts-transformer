package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.freeUdpPort;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.syntheticH264Idr;
import static org.tstrans.srt.SrtSenderParkSupport.NON_READING_PEER_KNOBS;
import static org.tstrans.srt.SrtSenderParkSupport.awaitParked;
import static org.tstrans.srt.SrtSenderParkSupport.connectManagedMux;
import static org.tstrans.srt.SrtSenderParkSupport.connectPlainMux;
import static org.tstrans.srt.SrtSenderParkSupport.dataBlob;
import static org.tstrans.srt.SrtSenderParkSupport.managedParkPolicy;
import static org.tstrans.srt.SrtSenderParkSupport.nullTsBlock;
import static org.tstrans.srt.SrtSenderParkSupport.peerListener;
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
 * The cancel-first {@code close()} contract of the srt SENDERS —
 * {@link ManagedMuxSender}, {@link ManagedSender}, {@link MuxSender},
 * {@link Sender}: {@code close()} from another thread wakes a parked send
 * instead of waiting behind it. Sender twin of
 * {@link SrtManagedCloseCancelsFirstTest} / {@link SrtPlainCloseCancelsFirstTest};
 * parity with the C ABI's {@code tst_managed_mux_sender_close}, which cancels
 * before tearing down.
 *
 * <p>Managed shells park in the Blocking reconnect's backoff wait after their
 * peer vanishes and end with {@code SrtException(CLOSED)}; plain shells park in
 * libsrt's blocking {@code srt_sendmsg} behind a peer that never reads and end
 * with {@code SrtException(CLOSED)} as well (the plain cancel closes the socket
 * under the parked send and the transport reports the cancel).
 *
 * <p>Under the previous contract {@code close()} took the resource lock the
 * parked send held and waited out the whole reconnect budget (managed) or the
 * peer (plain) — the bounded {@code get} on the close future is the red. Every
 * timeout path unparks the pump before failing so the daemon threads finish.
 */
class SrtSenderCloseCancelsFirstTest {

    private static final int LATENCY_MS = 120;

    private static ManagedSender connectManaged(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return ManagedSender.fromUrl(url, managedParkPolicy());
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    private static Sender connectPlain(String url, long budgetMs) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(budgetMs);
        SrtException last = null;
        while (System.nanoTime() < deadline) {
            try {
                return Sender.fromUrl(url);
            } catch (SrtException e) {
                last = e;
                Thread.sleep(50);
            }
        }
        throw new AssertionError("caller could not connect within " + budgetMs + " ms", last);
    }

    /**
     * {@code close()} while {@code sendVideo()} is parked in the reconnect
     * backoff on another thread ends that send with {@code SrtException(CLOSED)}
     * and returns.
     */
    @Test
    @Timeout(90)
    void closeWakesManagedMuxSenderParkedInReconnect() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        // conntimeo=300: each re-dial into the vanished listener fails in 300 ms,
        // so the parked time is the policy's 10 s backoff, not libsrt's 3 s
        // handshake timeout.
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300";

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        ManagedMuxSender sender = connectManagedMux(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);

        byte[] idr = syntheticH264Idr();
        AtomicLong pts = new AtomicLong();
        Pump pump = pump("parked-managed-mux-sender",
            () -> sender.sendVideo(idr, pts.getAndAdd(3_000), true), 10);
        Thread.sleep(300);            // a few frames flow over the live link
        peer.close();                 // peer vanishes: the next send breaks → Blocking reconnect
        awaitParked(pump, "sendVideo"); // ≥1.5 s inside ONE sendVideo = parked in the backoff wait

        // The call under test, from a side thread so a regression to the old
        // wait-behind-the-parked-send contract fails the bounded get below
        // instead of pinning this test thread until @Timeout.
        CompletableFuture<Void> closed = CompletableFuture.runAsync(sender::close);
        try {
            closed.get(2, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            // Nothing but its own two-attempt budget (~10 s) unparks the pre-fix
            // sender; wait it out so the daemon threads finish before the verdict.
            try {
                pump.end.get(30, TimeUnit.SECONDS);
            } catch (Exception ignored) {
                // see above
            }
            fail("close() blocked for >2 s behind the sendVideo parked in the reconnect backoff "
                + "instead of cancelling it");
            return;
        }

        Throwable end = pump.end.get(5, TimeUnit.SECONDS);
        pump.thread.join(TimeUnit.SECONDS.toMillis(2));
        assertTrue(end instanceof SrtException,
            "expected sendVideo to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a close-initiated cancel on the managed shell surfaces as CLOSED");
    }

    /** Same contract on the basic-bytes managed shell: a parked {@code sendBytes()} ends CLOSED. */
    @Test
    @Timeout(90)
    void closeWakesManagedSenderParkedInReconnect() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS + "&conntimeo=300";

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        ManagedSender sender = connectManaged(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);

        byte[] block = nullTsBlock();
        Pump pump = pump("parked-managed-sender", () -> sender.sendBytes(block), 10);
        Thread.sleep(300);
        peer.close();
        awaitParked(pump, "sendBytes");

        CompletableFuture<Void> closed = CompletableFuture.runAsync(sender::close);
        try {
            closed.get(2, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            try {
                pump.end.get(30, TimeUnit.SECONDS);
            } catch (Exception ignored) {
                // see above
            }
            fail("close() blocked for >2 s behind the sendBytes parked in the reconnect backoff "
                + "instead of cancelling it");
            return;
        }

        Throwable end = pump.end.get(5, TimeUnit.SECONDS);
        pump.thread.join(TimeUnit.SECONDS.toMillis(2));
        assertTrue(end instanceof SrtException,
            "expected sendBytes to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a close-initiated cancel on the managed shell surfaces as CLOSED");
    }

    /**
     * Plain {@link MuxSender}: {@code close()} while {@code sendData()} is parked
     * in {@code srt_sendmsg} (peer connected, never reading, no too-late drop)
     * ends that send with {@code SrtException(CLOSED)} and returns.
     */
    @Test
    @Timeout(90)
    void closeWakesPlainMuxSenderParkedInFullSendBuffer() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS
            + NON_READING_PEER_KNOBS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        MuxSender sender = connectPlainMux(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS); // never reads: its buffer, then ours, fill

        byte[] blob = dataBlob();
        AtomicLong pts = new AtomicLong();
        Pump pump = pump("parked-plain-mux-sender",
            () -> sender.sendData(blob, pts.getAndAdd(3_000)), 0);
        awaitParked(pump, "sendData");

        CompletableFuture<Void> closed = CompletableFuture.runAsync(sender::close);
        try {
            closed.get(2, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            // Rescue: the peer's shutdown breaks the parked srt_sendmsg.
            peer.close();
            try {
                pump.end.get(5, TimeUnit.SECONDS);
            } catch (Exception ignored) {
                // see above
            }
            fail("close() blocked for >2 s behind the sendData parked in srt_sendmsg "
                + "instead of cancelling it");
            return;
        }

        Throwable end = pump.end.get(5, TimeUnit.SECONDS);
        pump.thread.join(TimeUnit.SECONDS.toMillis(2));
        peer.close();
        assertTrue(end instanceof SrtException,
            "expected sendData to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a close-initiated cancel surfaces as CLOSED on the plain shells too (Arc 2)");
    }

    /** Plain {@link Sender}: a parked {@code sendBytes()} ends CLOSED and {@code close()} returns. */
    @Test
    @Timeout(90)
    void closeWakesPlainSenderParkedInFullSendBuffer() throws Exception {
        assumeTrue(isLinux(), "SRT live-socket test gated to Linux (same as the Rust/C twins)");
        int port = freeUdpPort();
        String listenUrl = "srt://:" + port + "?mode=listener&latency=" + LATENCY_MS
            + NON_READING_PEER_KNOBS;
        String callerUrl = "srt://127.0.0.1:" + port + "?latency=" + LATENCY_MS;

        CompletableFuture<Receiver> peerFuture = peerListener(listenUrl);
        Sender sender = connectPlain(callerUrl, 5_000);
        Receiver peer = peerFuture.get(5, TimeUnit.SECONDS);

        byte[] block = nullTsBlock();
        Pump pump = pump("parked-plain-sender", () -> sender.sendBytes(block), 0);
        awaitParked(pump, "sendBytes");

        CompletableFuture<Void> closed = CompletableFuture.runAsync(sender::close);
        try {
            closed.get(2, TimeUnit.SECONDS);
        } catch (TimeoutException te) {
            peer.close();
            try {
                pump.end.get(5, TimeUnit.SECONDS);
            } catch (Exception ignored) {
                // see above
            }
            fail("close() blocked for >2 s behind the sendBytes parked in srt_sendmsg "
                + "instead of cancelling it");
            return;
        }

        Throwable end = pump.end.get(5, TimeUnit.SECONDS);
        pump.thread.join(TimeUnit.SECONDS.toMillis(2));
        peer.close();
        assertTrue(end instanceof SrtException,
            "expected sendBytes to end with SrtException(CLOSED), got " + end);
        assertEquals(SrtException.Kind.CLOSED, ((SrtException) end).kind(),
            "a close-initiated cancel surfaces as CLOSED on the plain shells too (Arc 2)");
    }
}
