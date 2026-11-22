package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.*;
import static org.junit.jupiter.api.Assumptions.assumeTrue;
import static org.tstrans.TestSupport.isLinux;
import static org.tstrans.TestSupport.rtspHeader;
import static org.tstrans.TestSupport.roundtripConfig;
import static org.tstrans.TestSupport.syntheticH264Idr;

import java.io.DataInputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyFactory;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.spec.PKCS8EncodedKeySpec;
import java.util.Base64;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.time.Duration;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.shells.DemuxReceiver;

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

    /**
     * A peer that reads the whole ClientHello record (5-byte header + its
     * length) and then closes with a FIN ends {@code connect} with
     * {@code Kind.TLS}; the handshake loop used to poll the closed socket
     * forever. Reading the whole record matters: closing with unread bytes
     * would send RST, which already took the error path.
     */
    @Test
    void peerClosingMidHandshakeThrowsTls() throws Exception {
        assumeTrue(isLinux(), "live RTSP tests gated to Linux");
        try (ServerSocket listener = new ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))) {
            Thread peer = new Thread(() -> {
                try (Socket s = listener.accept()) {
                    s.setSoTimeout(5000);
                    DataInputStream in = new DataInputStream(s.getInputStream());
                    byte[] header = new byte[5];
                    in.readFully(header);
                    int len = ((header[3] & 0xff) << 8) | (header[4] & 0xff);
                    in.readFully(new byte[len]);
                } catch (Exception ignored) {
                    // the assertion below reports the failure
                }
            });
            peer.setDaemon(true);
            peer.start();

            String url = "rtsps://127.0.0.1:" + listener.getLocalPort() + "/x";
            RtspException ex = assertTimeoutPreemptively(Duration.ofSeconds(20),
                () -> assertThrows(RtspException.class,
                    () -> RtspClient.connect(RtspClientConfig.builder(url).build())),
                "connect did not return: the handshake loop is spinning on EOF");
            assertEquals(RtspException.Kind.TLS, ex.kind(), ex.getMessage());
            peer.join(5000);
        }
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
            int port = server.localAddr().orElseThrow(() -> new AssertionError("server must be bound"))
                .getPort();
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

    /**
     * An {@code rtsps://} server sends and takes media only inside its TLS connection unless
     * told otherwise: a reader's UDP SETUP reads 461 and binds no port, an interleaved one
     * 200; {@code allowUdpMedia(true)} serves UDP again.
     */
    @Test
    @Timeout(TIMEOUT_SEC)
    void rtspsServerRefusesUdpMediaUnlessAllowed() throws Exception {
        assumeTrue(isLinux(), "live RTSP tests gated to Linux");
        Path cert = fixtureDir().resolve("cert.pem");
        Path key = fixtureDir().resolve("key.pem");
        assumeTrue(Files.isRegularFile(cert) && Files.isRegularFile(key),
            "committed TLS fixture not reachable from test workingDir");
        SSLContext tls = trusting(cert);
        for (Boolean allow : new Boolean[] {null, true}) {
            var b = RtspServerConfig.builder()
                .bindAddr("rtsps://127.0.0.1:0")
                .tlsCert(cert.toString()).tlsKey(key.toString());
            if (allow != null) b.allowUdpMedia(allow);
            try (RtspServer server = RtspServer.start(b.build());
                 MountHandle mount = server.addUnicastMount("/live", roundtripConfig());
                 DatagramSocket media = new DatagramSocket(0, InetAddress.getLoopbackAddress())) {
                int port = server.localAddr().orElseThrow().getPort();
                String udp = RtspServerTest.readerSetup(tlsRaw(tls, port), port,
                    "RTP/AVP;unicast;client_port=" + media.getLocalPort() + "-"
                        + (media.getLocalPort() + 1));
                String tcp = RtspServerTest.readerSetup(tlsRaw(tls, port), port,
                    "RTP/AVP/TCP;unicast;interleaved=0-1");
                assertEquals(200, RtspServerTest.Raw.status(tcp), tcp);
                if (allow == null) {
                    assertEquals(461, RtspServerTest.Raw.status(udp), udp);
                    assertFalse(udp.contains("server_port"), udp);
                } else {
                    assertEquals(200, RtspServerTest.Raw.status(udp), udp);
                    assertTrue(rtspHeader(udp, "Transport").contains("server_port"), udp);
                }
            }
        }
    }

    /**
     * An {@code rtsps://} server refuses a multicast mount (it would send its media to the
     * group in the clear) unless UDP media is allowed.
     */
    @Test
    @Timeout(TIMEOUT_SEC)
    void rtspsServerRefusesAMulticastMountUnlessUdpMediaIsAllowed() throws Exception {
        assumeTrue(isLinux(), "live RTSP tests gated to Linux");
        Path cert = fixtureDir().resolve("cert.pem");
        Path key = fixtureDir().resolve("key.pem");
        assumeTrue(Files.isRegularFile(cert) && Files.isRegularFile(key),
            "committed TLS fixture not reachable from test workingDir");
        for (boolean allow : new boolean[] {false, true}) {
            var b = RtspServerConfig.builder()
                .bindAddr("rtsps://127.0.0.1:0")
                .tlsCert(cert.toString()).tlsKey(key.toString());
            if (allow) b.allowUdpMedia(true);
            try (RtspServer server = RtspServer.start(b.build())) {
                if (allow) {
                    try (MountHandle m = server.addMulticastMount("/mc", "239.0.0.1", 5004,
                            roundtripConfig())) {
                        assertEquals("multicast", m.mountKind());
                    }
                } else {
                    RtspException e = assertThrows(RtspException.class,
                        () -> server.addMulticastMount("/mc", "239.0.0.1", 5004,
                            roundtripConfig()));
                    assertEquals(RtspException.Kind.MOUNT, e.kind(), e.getMessage());
                    assertEquals(0, server.stats().mounts());
                }
            }
        }
    }

    /**
     * An {@code rtsps://} pull with the default configuration ({@link TransportPref#AUTO}, no
     * {@code transport} key) asks for TCP-interleaved media in its first SETUP: the media
     * stays inside the TLS connection. The peer is a TLS listener that records the SETUPs
     * and answers every request 200.
     */
    @Test
    @Timeout(TIMEOUT_SEC)
    void rtspsPullWithTheDefaultConfigAsksForInterleaved() throws Exception {
        assumeTrue(isLinux(), "live RTSP tests gated to Linux");
        Path cert = fixtureDir().resolve("cert.pem");
        Path key = fixtureDir().resolve("key.pem");
        assumeTrue(Files.isRegularFile(cert) && Files.isRegularFile(key),
            "committed TLS fixture not reachable from test workingDir");
        List<String> setups = new CopyOnWriteArrayList<>();
        try (SSLServerSocket listener = (SSLServerSocket) serving(cert, key)
                .getServerSocketFactory()
                .createServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            Thread peer = new Thread(() -> answerEverything(listener, setups), "rtsps-peer");
            peer.setDaemon(true);
            peer.start();
            String url = "rtsps://127.0.0.1:" + listener.getLocalPort() + "/x";
            var cfg = RtspClientConfig.builder(url)
                .tlsRootCertsPem(Files.readAllBytes(cert)).keepalive(false).build();
            assertEquals(TransportPref.AUTO, cfg.transportPref());
            try (RtspSession session = RtspClient.connect(cfg)) {
                assertFalse(session.isTornDown());
            }
            peer.join(TimeUnit.SECONDS.toMillis(5));
        }
        assertFalse(setups.isEmpty(), "no SETUP reached the peer");
        String first = RawRtspPeer.header(setups.get(0), "Transport");
        assertTrue(first.startsWith("RTP/AVP/TCP") && first.contains("interleaved="), first);
        assertEquals(1, setups.size(), () -> "more than one SETUP: " + setups);
    }

    /** One TLS connection: every request is answered 200; a SETUP is recorded and echoed. */
    private static void answerEverything(SSLServerSocket listener, List<String> setups) {
        try (Socket sock = listener.accept()) {
            sock.setSoTimeout(10_000);
            InputStream in = sock.getInputStream();
            OutputStream out = sock.getOutputStream();
            String req;
            while ((req = RawRtspPeer.readRequest(in)) != null) {
                String method = req.split(" ", 2)[0];
                String extra = "";
                byte[] body = new byte[0];
                switch (method) {
                    case "OPTIONS" ->
                        extra = "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n";
                    case "DESCRIBE" -> {
                        body = RawRtspPeer.MP2T_SDP.getBytes(StandardCharsets.US_ASCII);
                        extra = "Content-Type: application/sdp\r\nContent-Length: " + body.length
                            + "\r\n";
                    }
                    case "SETUP" -> {
                        setups.add(req);
                        extra = "Session: DEADBEEF;timeout=60\r\nTransport: "
                            + RawRtspPeer.header(req, "Transport") + "\r\n";
                    }
                    default -> extra = "Session: DEADBEEF\r\n";
                }
                out.write(("RTSP/1.0 200 OK\r\nCSeq: " + RawRtspPeer.header(req, "CSeq") + "\r\n"
                    + extra + "\r\n").getBytes(StandardCharsets.US_ASCII));
                out.write(body);
                out.flush();
            }
        } catch (Exception ignored) {
            // the client closed the connection; the assertions read what was recorded
        }
    }

    /** A TLS context that serves the committed fixture certificate and its PKCS#8 key. */
    private static SSLContext serving(Path cert, Path key) throws Exception {
        String pem = Files.readString(key)
            .replaceAll("-----[A-Z ]+-----", "").replaceAll("\\s", "");
        PKCS8EncodedKeySpec spec = new PKCS8EncodedKeySpec(Base64.getDecoder().decode(pem));
        PrivateKey pk;
        try {
            pk = KeyFactory.getInstance("EC").generatePrivate(spec);
        } catch (java.security.spec.InvalidKeySpecException notEc) {
            pk = KeyFactory.getInstance("RSA").generatePrivate(spec);
        }
        Certificate c;
        try (InputStream in = Files.newInputStream(cert)) {
            c = CertificateFactory.getInstance("X.509").generateCertificate(in);
        }
        KeyStore store = KeyStore.getInstance(KeyStore.getDefaultType());
        store.load(null, null);
        store.setKeyEntry("fixture", pk, new char[0], new Certificate[] {c});
        KeyManagerFactory kmf =
            KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(store, new char[0]);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(kmf.getKeyManagers(), null, null);
        return ctx;
    }

    /** A TLS context that trusts the committed self-signed fixture certificate alone. */
    private static SSLContext trusting(Path cert) throws Exception {
        KeyStore store = KeyStore.getInstance(KeyStore.getDefaultType());
        store.load(null, null);
        try (InputStream in = Files.newInputStream(cert)) {
            store.setCertificateEntry("fixture",
                CertificateFactory.getInstance("X.509").generateCertificate(in));
        }
        TrustManagerFactory tmf =
            TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(store);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, tmf.getTrustManagers(), null);
        return ctx;
    }

    private static RtspServerTest.Raw tlsRaw(SSLContext tls, int port) throws Exception {
        SSLSocket sock = (SSLSocket) tls.getSocketFactory().createSocket();
        sock.connect(new java.net.InetSocketAddress("127.0.0.1", port), 5000);
        sock.setSoTimeout(5000);
        sock.startHandshake();
        return new RtspServerTest.Raw(sock);
    }
}
