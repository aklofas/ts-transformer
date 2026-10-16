package org.tstrans.rtp;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

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
 * {@link RtspClientConfig#transportPref()} and {@link RtspClientConfig#rtspVersion()}
 * reach the wire. A scripted server answers OPTIONS and DESCRIBE, records the
 * SETUP request and refuses it, so the test reads exactly what the client asked
 * for without a media session.
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
}
