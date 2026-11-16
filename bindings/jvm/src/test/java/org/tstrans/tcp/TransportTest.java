package org.tstrans.tcp;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.scaledMs;
import static org.tstrans.shells.ShellPeers.within;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.net.InetAddress;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.cert.CertificateFactory;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.TcpException;
import org.tstrans.TestSupport;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.DemuxerConfig;
import org.tstrans.mpegts.Muxer;
import org.tstrans.mpegts.StrictMode;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.Receiver;
import org.tstrans.shells.Sender;
import org.tstrans.shells.ShellPeers.Cleanup;
import org.tstrans.shells.ShellPeers.Parked;
import org.tstrans.shells.ShellPeers.Watchdog;

@Timeout(90)
class TransportTest {

    private static final String LOOPBACK_V4 = "tcp://127.0.0.1:0?listen=1";
    private static final long PEER_WAIT_MS = 10_000;

    /** {@code n} TS packets on the null PID, packet {@code i} filled with {@code i}. */
    private static byte[] packets(int n) {
        byte[] out = new byte[n * 188];
        for (int i = 0; i < n; i++) {
            java.util.Arrays.fill(out, i * 188 + 4, (i + 1) * 188, (byte) i);
            out[i * 188] = 0x47;
            out[i * 188 + 1] = 0x1F;
            out[i * 188 + 2] = (byte) 0xFF;
            out[i * 188 + 3] = 0x10;
        }
        return out;
    }

    private static void assertKind(TcpException.Kind kind, Throwable e) {
        TcpException t = assertInstanceOf(TcpException.class, e);
        assertEquals(kind, t.kind(), t.getMessage());
    }

    private static Path resource(String name) throws Exception {
        return Path.of(TransportTest.class.getResource("/hls/" + name).toURI());
    }

    private static String tlsListenerUrl() throws Exception {
        return "tcps://127.0.0.1:0?listen=1&cert=" + resource("cert.pem") + "&key=" + resource("key.pem");
    }

    /** A client socket that trusts exactly the fixture certificate (SAN {@code 127.0.0.1}). */
    private static SSLSocket tlsClient(int port) throws Exception {
        KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
        ks.load(null, null);
        try (InputStream in = TransportTest.class.getResourceAsStream("/hls/cert.pem")) {
            ks.setCertificateEntry("tcp-test", CertificateFactory.getInstance("X.509").generateCertificate(in));
        }
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, tmf.getTrustManagers(), null);
        SSLSocket s = (SSLSocket) ctx.getSocketFactory().createSocket(InetAddress.getByName("127.0.0.1"), port);
        javax.net.ssl.SSLParameters p = s.getSSLParameters();
        p.setEndpointIdentificationAlgorithm("HTTPS");
        s.setSSLParameters(p);
        return s;
    }

    /** {@code accept()} bounded by a watchdog on the listener. */
    private static Transport accept(Listener l, Cleanup c) throws Exception {
        try (Watchdog w = new Watchdog(l.cancelHandle(), scaledMs(PEER_WAIT_MS))) {
            return c.add(l.accept());
        }
    }

    private static java.net.Socket plainClient(Listener l, Cleanup c) throws Exception {
        return c.add(new java.net.Socket(InetAddress.getByName("127.0.0.1"), l.localPort()));
    }

    /** Write {@code bytes} on a daemon thread (a TLS write blocks until the server handshakes), then close. */
    private static CompletableFuture<Void> writeAndClose(java.net.Socket s, byte[] bytes) {
        CompletableFuture<Void> done = new CompletableFuture<>();
        Thread t = new Thread(() -> {
            try (s) {
                s.getOutputStream().write(bytes);
                s.getOutputStream().flush();
                done.complete(null);
            } catch (Throwable e) {
                done.completeExceptionally(e);
            }
        }, "tcp-client-write");
        t.setDaemon(true);
        t.start();
        return done;
    }

    /** Receive until {@code want} bytes arrived, bounded by a watchdog on the receiver. */
    private static byte[] recvUntil(Receiver rx, int want) throws Exception {
        ByteArrayOutputStream got = new ByteArrayOutputStream();
        try (Watchdog w = new Watchdog(rx.cancelHandle(), scaledMs(PEER_WAIT_MS))) {
            while (got.size() < want) got.write(rx.recvBytes());
        }
        return got.toByteArray();
    }

    @Test
    void anAcceptedConnectionReceivesTsAsAReceiver() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            java.net.Socket client = plainClient(l, c);
            Receiver rx = c.add(accept(l, c).intoReceiver());
            byte[] sent = packets(70);
            client.getOutputStream().write(sent);
            client.close();
            ByteArrayOutputStream got = new ByteArrayOutputStream();
            TcpException end;
            try (Watchdog w = new Watchdog(rx.cancelHandle(), scaledMs(PEER_WAIT_MS))) {
                while (true) {
                    try {
                        got.write(rx.recvBytes());
                    } catch (TcpException e) {
                        end = e;
                        break;
                    }
                }
            }
            assertKind(TcpException.Kind.END_OF_STREAM, end);
            assertArrayEquals(sent, got.toByteArray());
            assertTrue(rx.localPort().isEmpty(), "an accepted connection reported a bound port");
        }
    }

    @Test
    void anAcceptedConnectionDemuxesAndCountsItsBytes() throws Exception {
        byte[] stream;
        try (Muxer m = new Muxer(TestSupport.roundtripConfig())) {
            for (int i = 0; i < 4; i++) m.pushVideo(TestSupport.syntheticH264Idr(), i * 3000L, true);
            stream = TestSupport.drain(m);
        }
        DemuxerConfig config = DemuxerConfig.builder().strictMode(StrictMode.OFF).build();
        for (boolean withConfig : new boolean[] {false, true}) {
            try (Cleanup c = new Cleanup()) {
                Listener l = c.add(Listener.open(LOOPBACK_V4));
                java.net.Socket client = plainClient(l, c);
                Transport t = accept(l, c);
                DemuxReceiver rx = c.add(withConfig ? t.intoDemuxReceiver(config) : t.intoDemuxReceiver());
                client.getOutputStream().write(stream);
                client.close();
                int maps = 0;
                int videos = 0;
                // The client's clean close ends the events with null.
                try (Watchdog w = new Watchdog(rx.cancelHandle(), scaledMs(PEER_WAIT_MS))) {
                    DemuxEvent e;
                    while ((e = rx.recvEvent()) != null) {
                        if (e instanceof DemuxEvent.ProgramMap) maps++;
                        if (e instanceof DemuxEvent.Video) videos++;
                    }
                }
                assertTrue(maps >= 1, "withConfig=" + withConfig + ": no ProgramMap");
                assertTrue(videos >= 1, "withConfig=" + withConfig + ": no Video");
                assertEquals(stream.length, rx.tsBytesDemuxed(), "withConfig=" + withConfig);
            }
        }
    }

    @Test
    void anAcceptedConnectionSendsAsASender() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            java.net.Socket client = plainClient(l, c);
            Sender tx = c.add(accept(l, c).intoSender());
            byte[] sent = packets(7);
            tx.sendBytes(sent);
            tx.flush();
            client.setSoTimeout((int) scaledMs(PEER_WAIT_MS));
            assertArrayEquals(sent, client.getInputStream().readNBytes(sent.length));
            tx.close();
            assertEquals(-1, client.getInputStream().read(), "bytes beyond those sent");
        }
    }

    @Test
    void aShellFromAnAcceptedConnectionOutlivesTheListener() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            java.net.Socket client = plainClient(l, c);
            Transport t = accept(l, c);
            l.close();
            Receiver rx = c.add(t.intoReceiver());
            byte[] sent = packets(8);
            client.getOutputStream().write(sent);
            assertArrayEquals(sent, recvUntil(rx, sent.length));
        }
    }

    @Test
    void intoConsumesTheTransport() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            plainClient(l, c);
            Transport t = accept(l, c);
            // A null config is refused before the take: the transport stays usable.
            assertThrows(NullPointerException.class, () -> t.intoDemuxReceiver(null));
            assertTrue(t.peerAddr().startsWith("127.0.0.1:"), t.peerAddr());
            c.add(t.intoReceiver());
            assertThrows(IllegalStateException.class, t::peerAddr);
            assertThrows(IllegalStateException.class, t::intoSender);
            assertThrows(IllegalStateException.class, t::intoReceiver);
            assertThrows(IllegalStateException.class, t::intoDemuxReceiver);
            assertThrows(IllegalStateException.class,
                () -> t.intoDemuxReceiver(DemuxerConfig.builder().build()));
            t.close();
            t.close();
        }
    }

    @Test
    void theShellThrowsTcpExceptions() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(LOOPBACK_V4));
            plainClient(l, c);
            plainClient(l, c);
            Receiver rx = c.add(accept(l, c).intoReceiver());
            rx.cancelHandle().cancel();
            assertKind(TcpException.Kind.CLOSED, assertThrows(TcpException.class, rx::recvBytes));
            Sender tx = c.add(accept(l, c).intoSender());
            tx.cancelHandle().cancel();
            assertKind(TcpException.Kind.CLOSED,
                assertThrows(TcpException.class, () -> tx.sendBytes(packets(1))));
        }
    }

    @Test
    void aTlsListenerServesATlsClient() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(tlsListenerUrl()));
            SSLSocket client = c.add(tlsClient(l.localPort()));
            Receiver rx = c.add(accept(l, c).intoReceiver());
            byte[] sent = packets(70);
            CompletableFuture<Void> written = writeAndClose(client, sent);
            assertArrayEquals(sent, recvUntil(rx, sent.length));
            written.get(scaledMs(PEER_WAIT_MS), TimeUnit.MILLISECONDS);
        }
    }

    @Test
    void aTlsListenerWithoutACertificateIsInvalidConfig() {
        assertKind(TcpException.Kind.INVALID_CONFIG,
            assertThrows(TcpException.class, () -> Listener.open("tcps://127.0.0.1:0?listen=1")));
    }

    @Test
    void aPlainClientOnATlsListenerFailsTheFirstReceive() throws Exception {
        try (Cleanup c = new Cleanup()) {
            Listener l = c.add(Listener.open(tlsListenerUrl()));
            java.net.Socket client = plainClient(l, c);
            Receiver rx = c.add(accept(l, c).intoReceiver());
            // Not a TLS record: the server's handshake must refuse it, not wait for more. The
            // failure surfaces through the receive path, which reports a dead transport.
            client.getOutputStream().write(packets(1));
            TcpException e;
            try (Watchdog w = new Watchdog(rx.cancelHandle(), scaledMs(PEER_WAIT_MS))) {
                e = assertThrows(TcpException.class, rx::recvBytes);
            }
            assertKind(TcpException.Kind.BROKEN, e);
            assertTrue(e.getMessage().contains("corrupt message"), e.getMessage());
        }
    }

    @Test
    void aSilentClientOnATlsListenerEndsWithTheCancel() throws Exception {
        for (boolean byClose : new boolean[] {false, true}) {
            try (Cleanup c = new Cleanup()) {
                Listener l = c.add(Listener.open(tlsListenerUrl()));
                // A plain client that sends nothing: the receive parks in the server's handshake.
                java.net.Socket client = plainClient(l, c);
                Receiver rx = c.add(accept(l, c).intoReceiver());
                // Closed before the receiver: if the cancel woke nothing, the client's EOF still
                // ends the handshake, so the test fails on its bound instead of hanging teardown.
                c.add(client);
                CancelHandle h = rx.cancelHandle();
                try (Parked p = new Parked(h, "a silent client completed a TLS handshake", rx::recvBytes)) {
                    long t0 = System.nanoTime();
                    if (byClose) {
                        within(scaledMs(3000), "close()", () -> {
                            rx.close();
                            return null;
                        });
                    } else {
                        h.cancel();
                    }
                    Throwable end = p.awaitEnd(scaledMs(3000));
                    long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
                    assertTrue(ms < scaledMs(3000), "byClose=" + byClose + ": ended after " + ms + " ms");
                    assertKind(TcpException.Kind.CLOSED, end);
                }
            }
        }
    }
}
