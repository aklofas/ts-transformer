package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.tstrans.TestSupport.waitUntil;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.tstrans.RtspException;

/**
 * The {@link RtspClientConfig} settings reach the wire. For the transport preference,
 * the RTSP version and the user agent a scripted server answers OPTIONS and DESCRIBE,
 * records the SETUP request and refuses it, so the test reads exactly what the client
 * asked for without a media session; the keepalive interval and the two timeouts run
 * against a {@link RawRtspPeer} session.
 */
class RtspClientPrefsTest {

    private static final String SDP =
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=t\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n"
        + "m=video 0 RTP/AVP 33\r\na=control:trackID=0\r\n";

    /** Serve one connection: OPTIONS, DESCRIBE, then record SETUP and answer 500. */
    private static Thread serve(ServerSocket ss, List<String> requests) {
        Thread t = new Thread(() -> {
            try (Socket s = ss.accept()) {
                InputStream in = s.getInputStream();
                OutputStream out = s.getOutputStream();
                while (true) {
                    String req = readRequest(in);
                    if (req == null) {
                        return;
                    }
                    requests.add(req);
                    String cseq = header(req, "CSeq");
                    String method = req.substring(0, req.indexOf(' '));
                    String resp;
                    switch (method) {
                        case "OPTIONS" -> resp = "RTSP/1.0 200 OK\r\nCSeq: " + cseq
                            + "\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n";
                        case "DESCRIBE" -> resp = "RTSP/1.0 200 OK\r\nCSeq: " + cseq
                            + "\r\nContent-Type: application/sdp\r\nContent-Length: " + SDP.length()
                            + "\r\n\r\n" + SDP;
                        default -> resp = "RTSP/1.0 500 Internal Server Error\r\nCSeq: " + cseq + "\r\n\r\n";
                    }
                    out.write(resp.getBytes(StandardCharsets.US_ASCII));
                    out.flush();
                    if (method.equals("SETUP")) {
                        return;
                    }
                }
            } catch (IOException ignored) {
                // the client hung up
            }
        });
        t.setDaemon(true);
        t.start();
        return t;
    }

    private static String readRequest(InputStream in) throws IOException {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        int b;
        while ((b = in.read()) >= 0) {
            buf.write(b);
            byte[] a = buf.toByteArray();
            int n = a.length;
            if (n >= 4 && a[n - 4] == '\r' && a[n - 3] == '\n' && a[n - 2] == '\r' && a[n - 1] == '\n') {
                return new String(a, StandardCharsets.US_ASCII);
            }
        }
        return null;
    }

    private static String header(String req, String name) {
        for (String line : req.split("\r\n")) {
            if (line.regionMatches(true, 0, name + ":", 0, name.length() + 1)) {
                return line.substring(name.length() + 1).trim();
            }
        }
        return "";
    }

    private static List<String> connectAndRecord(RtspClientConfigBuilder cfg) throws Exception {
        try (ServerSocket ss = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            List<String> requests = new CopyOnWriteArrayList<>();
            Thread server = serve(ss, requests);
            String url = "rtsp://127.0.0.1:" + ss.getLocalPort() + "/live";
            assertThrows(RtspException.class, () -> RtspClient.connect(cfg.build(url)));
            server.join(TimeUnit.SECONDS.toMillis(5));
            return requests;
        }
    }

    private interface RtspClientConfigBuilder {
        RtspClientConfig build(String url);
    }

    private static String setupTransport(List<String> requests) {
        String setup = requests.stream().filter(r -> r.startsWith("SETUP ")).findFirst().orElseThrow();
        return header(setup, "Transport");
    }

    @Test
    @Timeout(30)
    void transportPrefTcpAsksForInterleavedAtSetup() throws Exception {
        List<String> requests = connectAndRecord(url ->
            RtspClientConfig.builder(url).transportPref(TransportPref.TCP).build());
        String transport = setupTransport(requests);
        assertTrue(transport.contains("RTP/AVP/TCP"), "interleaved TCP requested, got: " + transport);
    }

    @Test
    @Timeout(30)
    void transportPrefUdpAsksForUdpAtSetup() throws Exception {
        List<String> requests = connectAndRecord(url ->
            RtspClientConfig.builder(url + "?transport=tcp").transportPref(TransportPref.UDP).build());
        String transport = setupTransport(requests);
        assertTrue(transport.contains("client_port"), "UDP requested, got: " + transport);
        assertTrue(!transport.contains("RTP/AVP/TCP"), "not interleaved, got: " + transport);
    }

    @Test
    @Timeout(30)
    void rtspVersionTwoIsSentOnTheRequestLine() throws Exception {
        List<String> requests = connectAndRecord(url ->
            RtspClientConfig.builder(url).rtspVersion(RtspVersion.V2_0).build());
        assertEquals("RTSP/2.0", requests.get(0).split("\r\n")[0].split(" ")[2]);
    }

    @Test
    @Timeout(30)
    void theUserAgentIsSent() throws Exception {
        List<String> requests = connectAndRecord(url ->
            RtspClientConfig.builder(url).userAgent("example-agent/1").build());
        assertTrue(!requests.isEmpty());
        for (String r : requests) assertEquals("example-agent/1", header(r, "User-Agent"), r);
    }

    /** A user agent that would split the header is refused before any byte is written. */
    @Test
    @Timeout(30)
    void aUserAgentWithACrLfIsRefusedBeforeConnecting() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, 200, 200)) {
            RtspClientConfig cfg = RtspClientConfig.builder(peer.url())
                .userAgent("x\r\nAuthorization: Basic injected").build();
            RtspException e = assertThrows(RtspException.class, () -> RtspClient.connect(cfg));
            assertEquals(RtspException.Kind.PROTOCOL, e.kind());
            assertTrue(peer.requests.isEmpty(), "a request was written: " + peer.requests);
        }
    }

    @Test
    @Timeout(30)
    void anExplicitIntervalSendsAPingWithinIt() throws Exception {
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, 200, 200)) {
            RtspClientConfig cfg = RtspClientConfig.builder(peer.url())
                .transportPref(TransportPref.TCP).keepaliveIntervalMs(200).build();
            try (RtspSession s = RtspClient.connect(cfg)) {
                // The SETUP reply says timeout=60, so without the interval the first ping
                // would come after 30 s.
                assertTrue(waitUntil(() -> pingsAfterPlay(peer.requests) >= 2, org.tstrans.TestSupport.scaledMs(5_000)),
                    () -> "no pings after PLAY: " + methods(peer.requests));
            }
        }
    }

    @Test
    @Timeout(30)
    void aRequestTheServerNeverAnswersEndsAtTheRequestTimeout() throws Exception {
        // The peer reads a PAUSE and never answers it.
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.MP2T_SDP, false, null, 200)) {
            RtspClientConfig cfg = RtspClientConfig.builder(peer.url())
                .transportPref(TransportPref.TCP).keepalive(false).requestTimeoutMs(300).build();
            try (RtspSession s = RtspClient.connect(cfg)) {
                long t0 = System.nanoTime();
                RtspException e = assertThrows(RtspException.class, s::pause);
                long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
                assertEquals(RtspException.Kind.TIMEOUT, e.kind());
                assertTrue(peer.awaitPause(0), "the PAUSE was written");
                // The default deadline is 10 s: far above this bound.
                assertTrue(ms < org.tstrans.TestSupport.scaledMs(5_000), "took " + ms + " ms");
            }
        }
    }

    /**
     * {@code connectH264} passes the four settings in their places: three distinct
     * millisecond values, so two swapped arguments show as a missing ping, an early
     * timeout or a late one.
     */
    @Test
    @Timeout(30)
    void connectH264HonoursEachSetting() throws Exception {
        // The peer reads a PAUSE and never answers it.
        try (RawRtspPeer peer = new RawRtspPeer(RawRtspPeer.H264_SDP, false, null, 200)) {
            RtspClientConfig cfg = RtspClientConfig.builder(peer.url())
                .transportPref(TransportPref.TCP).keepaliveIntervalMs(200)
                .connectTimeoutMs(9_000).requestTimeoutMs(4_000)
                .userAgent("example-agent/1").build();
            try (RtspSession s = RtspClient.connectH264(cfg)) {
                assertTrue(waitUntil(() -> pingsAfterPlay(peer.requests) >= 2, org.tstrans.TestSupport.scaledMs(5_000)),
                    () -> "no pings after PLAY: " + methods(peer.requests));
                long t0 = System.nanoTime();
                RtspException e = assertThrows(RtspException.class, s::pause);
                long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
                assertEquals(RtspException.Kind.TIMEOUT, e.kind());
                // A deadline never fires early; the 9 s connect budget would be later still.
                assertTrue(ms >= 3_500, "the request deadline fired after " + ms + " ms");
                assertTrue(ms < org.tstrans.TestSupport.scaledMs(8_000), "took " + ms + " ms");
            }
            for (String r : peer.requests) assertEquals("example-agent/1", header(r, "User-Agent"), r);
        }
    }

    /** An {@code rtsps://} server that accepts and never answers the handshake. */
    @Test
    @Timeout(30)
    void aHandshakeTheServerNeverAnswersEndsAtTheConnectTimeout() throws Exception {
        try (ServerSocket ss = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
            RtspClientConfig cfg = RtspClientConfig.builder(
                    "rtsps://127.0.0.1:" + ss.getLocalPort() + "/x")
                .connectTimeoutMs(300).build();
            long t0 = System.nanoTime();
            RtspException e = assertThrows(RtspException.class, () -> RtspClient.connect(cfg));
            long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - t0);
            assertEquals(RtspException.Kind.IO, e.kind(), e.getMessage());
            // The default budget is 10 s: far above this bound.
            assertTrue(ms < org.tstrans.TestSupport.scaledMs(5_000), "took " + ms + " ms");
        }
    }

    /** OPTIONS or GET_PARAMETER requests read after the PLAY. */
    static long pingsAfterPlay(List<String> requests) {
        List<String> m = methods(requests);
        int play = m.indexOf("PLAY");
        if (play < 0) return 0;
        return m.subList(play + 1, m.size()).stream()
            .filter(x -> x.equals("OPTIONS") || x.equals("GET_PARAMETER")).count();
    }

    static List<String> methods(List<String> requests) {
        return requests.stream().map(r -> r.split(" ", 2)[0]).toList();
    }

}
