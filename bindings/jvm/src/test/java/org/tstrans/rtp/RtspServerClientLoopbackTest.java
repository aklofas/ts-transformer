package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.sha256Units;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.ByteArrayOutputStream;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.mpegts.Muxer;
import org.tstrans.shells.DemuxReceiver;

/**
 * Capstone live cross-binding loopback: an in-JVM {@link RtspServer} (mount fed by
 * the push family) ←→ {@link RtspClient}. The client connects, drives OPTIONS/
 * DESCRIBE/SETUP/PLAY, takes the data plane via {@link RtspSession#intoDemuxReceiver},
 * and the first demuxed Video sample's SHA must equal an OFFLINE Muxer→Demuxer
 * reference (self-validating, no committed golden). This is the genuine RTSP
 * cross-binding parity proof tst-py could not do (its fixture server is cross-crate
 * {@code #[cfg(test)]}-only). Linux-gated; deterministic + watchdog-bounded.
 *
 * <h2>Hang-proofing</h2>
 * The {@link RtspServer} + mount + a <b>continuous producer daemon</b> live on the
 * main thread (server methods don't block). The client work runs on a daemon thread
 * completing a {@link CompletableFuture}; a watchdog daemon {@code close()}s the
 * obtained {@link DemuxReceiver} after the ceiling to unwedge a parked recv (the rtp
 * convenience wrapper has no {@code cancelHandle}; {@code close()} cancels-first then
 * frees — the proven safe path). Cleanup joins the client/producer threads
 * before the final free so there is provably no in-flight {@code nNext} at teardown.
 */
class RtspServerClientLoopbackTest {

    private static final int TIMEOUT_SEC = 25;

    @Test
    @Timeout(TIMEOUT_SEC + 10)
    void serverToClientLoopbackIsCrossBindingConsistent() throws Exception {
        assumeTrue(isLinux(),
            "RTSP live loopback gated to Linux (real sockets + tokio runtime)");

        String offlineSha = offlineSha();

        try (RtspServer server = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            String addr = server.localAddr();
            assertNotNull(addr, "server must be bound");
            int port = Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
            MountHandle mount = server.addUnicastMount("/live", roundtripConfig());

            // Continuous producer: keep pushing identical IDRs so post-PLAY bytes
            // always flow (no push-then-close, which would discard the batch before
            // delivery). Stops when `done` fires.
            CountDownLatch done = new CountDownLatch(1);
            Thread producer = new Thread(() -> {
                long pts = 0;
                try {
                    while (done.getCount() > 0) {
                        mount.pushVideo(syntheticH264Idr(), pts, true);
                        mount.flush();
                        pts += 3000;
                        Thread.sleep(20);
                    }
                } catch (Exception ignored) {
                    // server stop / mount close races teardown — benign
                }
            });
            producer.setDaemon(true);
            producer.start();

            // Client + receiver on a daemon thread → SHA future. The receiver is
            // published to rxRef the moment it is obtained so the watchdog/cleanup
            // can close() it cross-thread on the failure path.
            AtomicReference<DemuxReceiver> rxRef = new AtomicReference<>();
            CompletableFuture<String> shaFuture = new CompletableFuture<>();
            String url = "rtsp://127.0.0.1:" + port + "/live";
            Thread client = new Thread(() -> {
                try {
                    RtspSession session = RtspClient.connect(RtspClientConfig.of(url));
                    DemuxReceiver rx = session.intoDemuxReceiver();
                    rxRef.set(rx);
                    // The data plane is a consumable handle: a second take is
                    // CLOSED, as in Python.
                    RtspException twice = assertThrows(RtspException.class,
                        session::intoDemuxReceiver);
                    assertEquals("CLOSED", twice.kind().name());
                    String sha = null;
                    try {
                        for (DemuxEvent e : rx) {
                            if (e instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                                sha = sha256Units(v.parse());
                                break;
                            }
                        }
                    } catch (RuntimeException re) {
                        // The rtp DemuxReceiver iterator wraps checked
                        // RtpException/DemuxException in a RuntimeException. A
                        // CANCELLED RtpException = the watchdog/teardown close() fired.
                        Throwable cause = re.getCause();
                        if (cause instanceof org.tstrans.RtpException || cause instanceof RtspException) {
                            // fall through: sha may still be null → fail below
                        } else {
                            throw re;
                        }
                    }
                    // The consumed check runs before the H.264
                    // wrong-constructor check, so taking the data plane the
                    // other way is CLOSED too (it was PROTOCOL). After the
                    // stream check: this consuming call tears the session down.
                    RtspException otherTake = assertThrows(RtspException.class,
                        session::intoH264Receiver);
                    assertEquals("CLOSED", otherTake.kind().name());
                    if (sha == null) {
                        shaFuture.completeExceptionally(
                            new AssertionError("no typed Video event arrived"));
                    } else {
                        shaFuture.complete(sha);
                    }
                } catch (Throwable t) {
                    shaFuture.completeExceptionally(t);
                }
            });
            client.setDaemon(true);
            client.start();

            // Watchdog: close the receiver after the ceiling so a parked next()
            // can never wedge the gating runners. close() cancels-first then frees.
            Thread watchdog = new Thread(() -> {
                try {
                    Thread.sleep(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC - 3));
                    if (!shaFuture.isDone()) {
                        DemuxReceiver rx = rxRef.get();
                        if (rx != null) {
                            rx.close();
                        }
                    }
                } catch (InterruptedException ignored) {
                    // Happy path: interrupted by the finally before the ceiling.
                }
            });
            watchdog.setDaemon(true);
            watchdog.start();

            String liveSha;
            try {
                liveSha = shaFuture.get(TIMEOUT_SEC, TimeUnit.SECONDS);
            } catch (Exception e) {
                throw new AssertionError("client thread failed to complete", e);
            } finally {
                done.countDown();
                watchdog.interrupt();
                // Unwedge any still-parked recv (error/timeout path) so the daemon
                // can exit, join it BEFORE the final close so there is provably no
                // concurrent native nNext when we free the handle. close() is
                // idempotent → no-op on the happy/watchdog paths. Bounded joins so a
                // wedged recv can't hang the test.
                DemuxReceiver rx = rxRef.get();
                if (rx != null && !shaFuture.isDone()) {
                    rx.close();
                }
                client.join(TimeUnit.SECONDS.toMillis(5));
                producer.join(TimeUnit.SECONDS.toMillis(2));
                // Close the mount after the producer thread has stopped so there
                // is no in-flight push. MountHandle.close() is idempotent.
                mount.close();
                if (rx != null) {
                    if (client.isAlive()) {
                        System.err.println(
                            "WARN: client thread still alive after close+join; skipping close to avoid UAF");
                    } else {
                        rx.close();
                    }
                }
            }

            assertEquals(offlineSha, liveSha,
                "live RtspServer→RtspClient→DemuxReceiver path must demux to the same "
                    + "video payload SHA as the offline Muxer→Demuxer path");
        }
    }

    @Test
    @Timeout(TIMEOUT_SEC + 10)
    void tcpTransportPreferenceStillPlays() throws Exception {
        assumeTrue(isLinux(),
            "RTSP live loopback gated to Linux (real sockets + tokio runtime)");

        try (RtspServer server = RtspServer.start(RtspServerConfig.of("127.0.0.1:0"))) {
            String addr = server.localAddr();
            int port = Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
            MountHandle mount = server.addUnicastMount("/live", roundtripConfig());

            CountDownLatch done = new CountDownLatch(1);
            Thread producer = new Thread(() -> {
                long pts = 0;
                try {
                    while (done.getCount() > 0) {
                        mount.pushVideo(syntheticH264Idr(), pts, true);
                        mount.flush();
                        pts += 3000;
                        Thread.sleep(20);
                    }
                } catch (Exception ignored) {
                    // server stop / mount close races teardown
                }
            });
            producer.setDaemon(true);
            producer.start();

            RtspClientConfig cfg = RtspClientConfig.builder("rtsp://127.0.0.1:" + port + "/live")
                .transportPref(TransportPref.TCP).build();
            RtspSession session = RtspClient.connect(cfg);
            DemuxReceiver rx = session.intoDemuxReceiver();
            // A parked read must not wedge the runner: close() cancels first.
            Thread watchdog = new Thread(() -> {
                try {
                    Thread.sleep(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC - 3));
                    rx.close();
                } catch (InterruptedException ignored) {
                    // happy path
                }
            });
            watchdog.setDaemon(true);
            watchdog.start();
            try {
                boolean sawVideo = false;
                for (DemuxEvent e : rx) {
                    if (e instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                        sawVideo = true;
                        break;
                    }
                }
                assertTrue(sawVideo, "a Video event arrived over the interleaved TCP session");
            } finally {
                done.countDown();
                watchdog.interrupt();
                producer.join(TimeUnit.SECONDS.toMillis(2));
                rx.close();
                mount.close();
            }
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────

    private static String offlineSha() throws Exception {
        ByteArrayOutputStream acc = new ByteArrayOutputStream();
        byte[] out = new byte[8192];
        try (Muxer m = new Muxer(roundtripConfig())) {
            for (int i = 0; i < 24; i++) {
                m.pushVideo(syntheticH264Idr(), i * 3000L, true);
            }
            int n;
            while ((n = m.pull(out)) > 0) acc.write(out, 0, n);
        }
        try (Demuxer d = new Demuxer()) {
            d.feed(acc.toByteArray());
            d.flush();
            for (DemuxEvent e : d) {
                if (e instanceof DemuxEvent.Video v && !v.parse().isEmpty()) {
                    return sha256Units(v.parse());
                }
            }
        }
        throw new AssertionError("offline reference produced no Video event");
    }
}
