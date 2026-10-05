package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

/**
 * rtsps:// end-to-end: server bound with the committed CA:FALSE fixture
 * cert/key (file paths), client trusting the
 * self-signed leaf via {@code tlsRootCertsPem}. Mirrors tst-py's
 * {@code test_rtp_integration.py} rtsps test; hang-proofing conventions
 * (producer daemon / client daemon / watchdog) follow
 * {@link RtspServerClientLoopbackTest}.
 *
 * <p>The fixture lives under the Python binding tree — single committed
 * source, no copy-drift. Cross-tree relative path from the gradle project
 * dir; skipped (assumption) if run from a layout where it isn't present.
 */
class RtspTlsTest {

    private static final int TIMEOUT_SEC = 25;

    private static Path fixtureDir() {
        return Path.of("..", "python", "tests", "fixtures", "tls")
            .toAbsolutePath().normalize();
    }

    private static byte[] idr() {
        byte[] b = new byte[20];
        b[0] = 0; b[1] = 0; b[2] = 0; b[3] = 1; b[4] = 0x65;
        for (int i = 0; i < 15; i++) b[5 + i] = (byte) (0xA5 ^ i);
        return b;
    }

    private static MuxerConfig cfg() {
        return MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000).addVideo(0x1011, VideoCodec.H264).build();
    }

    @Test
    void badCertPathThrowsTlsFromStart() {
        assumeTrue(isLinux(), "live RTSP tests gated to Linux");
        var cfg = RtspServerConfig.builder()
            .bindAddr("rtsps://127.0.0.1:0")
            .tlsCert("/nonexistent/tstrans/cert.pem")
            .tlsKey("/nonexistent/tstrans/key.pem")
            .build();
        RtspException ex = assertThrows(RtspException.class, () -> RtspServer.start(cfg));
        assertEquals(RtspException.Kind.TLS, ex.kind());
    }

    @Test
    void garbageRootsPemThrowsTlsBeforeAnyIo() {
        // Mirrors tst-py test_rtsp_client.py: roots parsing fails before
        // the client opens a socket, so no server is needed.
        var cfg = RtspClientConfig.builder("rtsps://127.0.0.1:1/never")
            .tlsRootCertsPem("not a pem".getBytes())
            .build();
        RtspException ex = assertThrows(RtspException.class, () -> RtspClient.connect(cfg));
        assertEquals(RtspException.Kind.TLS, ex.kind());
        assertTrue(ex.getMessage().contains("no certificates"));
    }

    @Test
    void corruptPemBodyThrowsTlsBeforeAnyIo() {
        // A BEGIN marker with a garbage body exercises the invalid-PEM
        // branch of the roots parser (vs. the zero-certs branch above,
        // which rustls-pemfile reaches by silently skipping marker-less
        // text). No server involved — fails before any I/O.
        byte[] corrupt = ("-----BEGIN CERTIFICATE-----\n"
            + "!!!! this is not base64 !!!!\n"
            + "-----END CERTIFICATE-----\n").getBytes();
        var cfg = RtspClientConfig.builder("rtsps://127.0.0.1:1/never")
            .tlsRootCertsPem(corrupt)
            .build();
        RtspException ex = assertThrows(RtspException.class, () -> RtspClient.connect(cfg));
        assertEquals(RtspException.Kind.TLS, ex.kind());
        assertTrue(ex.getMessage().contains("invalid PEM"));
    }

    @Test
    @Timeout(TIMEOUT_SEC + 10)
    void rtspsLoopbackDeliversVideo() throws Exception {
        assumeTrue(isLinux(), "live RTSP tests gated to Linux");
        Path cert = fixtureDir().resolve("cert.pem");
        Path key = fixtureDir().resolve("key.pem");
        assumeTrue(Files.isRegularFile(cert) && Files.isRegularFile(key),
            "committed TLS fixture not reachable from test workingDir");
        byte[] trustAnchor = Files.readAllBytes(cert);

        var serverCfg = RtspServerConfig.builder()
            .bindAddr("rtsps://127.0.0.1:0")
            .tlsCert(cert.toString()).tlsKey(key.toString())
            .build();
        try (RtspServer server = RtspServer.start(serverCfg)) {
            String addr = server.localAddr();
            assertNotNull(addr, "server must be bound");
            int port = Integer.parseInt(addr.substring(addr.lastIndexOf(':') + 1));
            MountHandle mount = server.addUnicastMount("/live", cfg());

            CountDownLatch done = new CountDownLatch(1);
            Thread producer = new Thread(() -> {
                long pts = 0;
                try {
                    while (done.getCount() > 0) {
                        mount.pushVideo(idr(), pts, true);
                        mount.flush();
                        pts += 3000;
                        Thread.sleep(20);
                    }
                } catch (Exception ignored) {
                    // teardown race — benign
                }
            });
            producer.setDaemon(true);
            producer.start();

            AtomicReference<DemuxReceiver> rxRef = new AtomicReference<>();
            CompletableFuture<Boolean> sawVideo = new CompletableFuture<>();
            // SAN covers both DNS:localhost and IP:127.0.0.1 — dial the IP
            // form, same as the Python rtsps integration test.
            String url = "rtsps://127.0.0.1:" + port + "/live";
            Thread client = new Thread(() -> {
                try {
                    var clientCfg = RtspClientConfig.builder(url)
                        .tlsRootCertsPem(trustAnchor)
                        .build();
                    RtspSession session = RtspClient.connect(clientCfg);
                    DemuxReceiver rx = session.intoDemuxReceiver();
                    rxRef.set(rx);
                    boolean found = false;
                    try {
                        for (DemuxEvent e : rx) {
                            if (e instanceof DemuxEvent.Video) {
                                found = true;
                                break;
                            }
                        }
                    } catch (RuntimeException re) {
                        // watchdog close() surfaces as a wrapped cancel — fall
                        // through; `found` decides pass/fail.
                        if (!(re.getCause() instanceof org.tstrans.RtpException
                                || re.getCause() instanceof RtspException)) {
                            throw re;
                        }
                    }
                    sawVideo.complete(found);
                } catch (Throwable t) {
                    sawVideo.completeExceptionally(t);
                }
            });
            client.setDaemon(true);
            client.start();

            Thread watchdog = new Thread(() -> {
                try {
                    Thread.sleep(TimeUnit.SECONDS.toMillis(TIMEOUT_SEC - 3));
                    if (!sawVideo.isDone()) {
                        DemuxReceiver rx = rxRef.get();
                        if (rx != null) rx.close();
                    }
                } catch (Exception ignored) { }
            });
            watchdog.setDaemon(true);
            watchdog.start();

            try {
                assertTrue(sawVideo.get(TIMEOUT_SEC, TimeUnit.SECONDS),
                    "no Video event arrived over the rtsps:// channel");
            } finally {
                done.countDown();
                DemuxReceiver rx = rxRef.get();
                if (rx != null) rx.close();
                client.join(TimeUnit.SECONDS.toMillis(5));
                producer.join(TimeUnit.SECONDS.toMillis(5));
            }
        }
    }
}
