package org.tstrans.srt;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.SrtException;
import org.tstrans.mpegts.DemuxEvent;

/**
 * Live-socket tests for {@code lastSeenMicros(int pid)} (task D7) on the two
 * srt convenience receivers: the plain {@link DemuxReceiver} and the managed
 * (auto-reconnect) {@link ManagedDemuxReceiver}. Each proves the same three
 * states as the rtp {@code DemuxReceiverTest} counterpart: {@code null}
 * before any event has been demuxed, {@code null} for an unrecognized PID,
 * and a positive Unix-epoch microsecond count for the configured video PID
 * once at least one event has arrived.
 *
 * <p>Both fixtures below reuse already-proven live-socket shapes from sibling
 * test files rather than inventing new topology: {@link #connectedReceiver}
 * mirrors {@code DemuxReceiverCloseRaceTest}'s helper (a continuous
 * caller-mode sender daemon streams into a listener accepted on this thread),
 * and {@link #managedDemuxReceiverLastSeenMicrosTracksLiveStreamAndNullsUnknownPid}
 * mirrors {@code SrtManagedLiveTest}'s Test 3 topology (managed shell as the
 * active CALLER on the main thread; peer is a plain listener+{@code
 * MuxSender} on a daemon thread streaming continuously until observed).
 *
 * <h2>Platform gate</h2>
 * Every test opens real SRT sockets and is gated to Linux only via {@link
 * org.junit.jupiter.api.Assumptions#assumeTrue} — identical to the other srt
 * live-socket test files.
 */
class LastSeenMicrosTest {

    private static final int LATENCY_MS = 120;

    /** Timeout in seconds for inter-thread signalling in the managed-shell test. */
    private static final int TIMEOUT_SEC = 15;

    private static boolean isCleanEndOfStream(RuntimeException re) {
        Throwable cause = re.getCause();
        return cause instanceof SrtException se
            && (se.kind() == SrtException.Kind.CLOSED || se.kind() == SrtException.Kind.BROKEN);
    }

    // ── srt.DemuxReceiver (plain) ──────────────────────────────────────────

    /**
     * Stand up a loopback and hand back a CONNECTED {@link DemuxReceiver} (on
     * this thread) plus a daemon that keeps a caller-mode sender streaming so
     * the receiver stays live. Returns the receiver; the caller owns closing
     * it and flipping {@code stop} afterward. Adapted from {@code
     * DemuxReceiverCloseRaceTest#connectedReceiver}.
     */
    private DemuxReceiver connectedReceiver(AtomicReference<Boolean> stop) throws Exception {
        Listener listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
            .listener().listen();
        int port = listener.localAddr().port();

        Thread sender = new Thread(() -> {
            try (MuxSender tx = MuxSender.fromUrl(
                    "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS,
                    roundtripConfig())) {
                long pts = 0;
                while (!stop.get()) {
                    tx.sendVideo(syntheticH264Idr(), pts, true);
                    pts += 3003;
                    Thread.sleep(5);
                }
            } catch (Throwable ignored) {
                // Sender teardown races receiver close; any error here is benign.
            }
        }, "last-seen-micros-sender");
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
    void demuxReceiverLastSeenMicrosTracksLiveStreamAndNullsUnknownPid() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");

        AtomicReference<Boolean> stop = new AtomicReference<>(false);
        CountDownLatch observed = new CountDownLatch(1);
        try (DemuxReceiver rx = connectedReceiver(stop)) {
            // The sender daemon is already streaming, but nothing has been pulled
            // through recvEvent() yet — the demuxer's per-stream stats are only
            // updated as events are produced, so both queries must read null here.
            assertNull(rx.lastSeenMicros(0x1011),
                "lastSeenMicros must be null before any event has been demuxed");
            assertNull(rx.lastSeenMicros(0x1FFF),
                "lastSeenMicros must be null for an unrecognized PID");

            // Hard no-hang safety net, the same shape as the managed test below:
            // JUnit's @Timeout cannot interrupt a thread parked in a native
            // recv, so a daemon watchdog cancels through a handle obtained
            // BEFORE iterating. A starved receiver then fails the Video
            // assertion loudly instead of wedging the gating runner.
            CancelHandle cancel = rx.cancelHandle();
            Thread watchdog = new Thread(() -> {
                try {
                    if (!observed.await(TIMEOUT_SEC - 3, TimeUnit.SECONDS)) {
                        cancel.cancel();
                    }
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            }, "last-seen-micros-watchdog");
            watchdog.setDaemon(true);
            watchdog.start();

            // The first demuxed event is typically a ProgramMap (PAT/PMT), not a
            // Video sample — pull until a Video event on the configured PID
            // arrives (bounded so a real regression fails instead of hanging).
            DemuxEvent.Video videoEvent = null;
            int pulled = 0;
            try {
                for (DemuxEvent e : rx) {
                    pulled++;
                    if (e instanceof DemuxEvent.Video v) {
                        videoEvent = v;
                        break;
                    }
                    if (pulled >= 20) break;
                }
            } catch (RuntimeException re) {
                // A watchdog cancel surfaces as CLOSED/BROKEN; let the Video
                // assertion below report the starvation instead.
                if (!isCleanEndOfStream(re)) throw re;
            } finally {
                observed.countDown(); // always release the watchdog
            }
            assertNotNull(videoEvent, "expected a Video event on the configured PID");

            Long seen = rx.lastSeenMicros(0x1011);
            assertNotNull(seen, "lastSeenMicros must be non-null for the video PID after delivery");
            assertTrue(seen > 0, "lastSeenMicros must be a positive Unix-epoch microsecond count");

            assertNull(rx.lastSeenMicros(0x1FFF),
                "lastSeenMicros must stay null for a PID that was never carried");
        } finally {
            stop.set(true);
        }
    }

    // ── srt.ManagedDemuxReceiver ────────────────────────────────────────────

    @Test
    @Timeout(TIMEOUT_SEC + 10)
    void managedDemuxReceiverLastSeenMicrosTracksLiveStreamAndNullsUnknownPid() throws Exception {
        assumeTrue(isLinux(), "srt live-socket loopback gated to Linux");

        // Topology mirrors SrtManagedLiveTest's Test 3 (the proven deterministic
        // shape): the managed shell is the active CALLER on the MAIN thread; the
        // peer is a plain listener+MuxSender on a daemon thread that streams
        // CONTINUOUSLY until main signals `observed` (closes the starved-receiver
        // window that would otherwise flake this test on loaded CI runners).
        CompletableFuture<Integer> portFuture = new CompletableFuture<>();
        CountDownLatch observed = new CountDownLatch(1);

        Thread peerThread = new Thread(() -> {
            Listener listener = null;
            Socket sock = null;
            MuxSender ms = null;
            try {
                listener = new Builder("srt://127.0.0.1:0?mode=listener&latency=" + LATENCY_MS)
                    .listener()
                    .listen();
                portFuture.complete(listener.localAddr().port());

                sock = listener.accept(null);
                ms = sock.intoMuxSender(roundtripConfig());
                long pts = 0;
                for (int round = 0; round < 400 && observed.getCount() > 0; round++) {
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

        int port;
        try {
            port = portFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
        } catch (Exception e) {
            peerThread.interrupt();
            throw new AssertionError("peer thread failed to publish port", e);
        }

        boolean sawEvent = false;
        try (ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl(
                "srt://127.0.0.1:" + port + "?mode=caller&latency=" + LATENCY_MS)) {
            // Constructed but not yet iterated — nothing has been demuxed.
            assertNull(rx.lastSeenMicros(0x1011),
                "lastSeenMicros must be null before any event has been demuxed");
            assertNull(rx.lastSeenMicros(0x1FFF),
                "lastSeenMicros must be null for an unrecognized PID");

            // Hard no-hang safety net (same as SrtManagedLiveTest): a cancel handle
            // obtained BEFORE iterating — the documented happy-path moment.
            CancelHandle cancel = rx.cancelHandle();
            Thread watchdog = new Thread(() -> {
                try {
                    if (!observed.await(TIMEOUT_SEC - 3, TimeUnit.SECONDS)) {
                        cancel.cancel();
                    }
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            });
            watchdog.setDaemon(true);
            watchdog.start();
            // The first demuxed event is typically a ProgramMap (PAT/PMT), not a
            // Video sample — pull until a Video event on the configured PID
            // arrives (bounded so a real regression fails instead of hanging).
            try {
                int pulled = 0;
                for (DemuxEvent e : rx) {
                    sawEvent = true;
                    pulled++;
                    if (e instanceof DemuxEvent.Video || pulled >= 20) break;
                }
            } catch (RuntimeException re) {
                if (!isCleanEndOfStream(re)) throw re;
            } finally {
                observed.countDown(); // always release the peer + watchdog
            }

            assertTrue(sawEvent, "managed demux receiver must have seen at least one demux event");
            Long seen = rx.lastSeenMicros(0x1011);
            assertNotNull(seen, "lastSeenMicros must be non-null for the video PID after delivery");
            assertTrue(seen > 0, "lastSeenMicros must be a positive Unix-epoch microsecond count");

            assertNull(rx.lastSeenMicros(0x1FFF),
                "lastSeenMicros must stay null for a PID that was never carried");
        }

        peerThread.join(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC));
    }
}
